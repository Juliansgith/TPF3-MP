#!/usr/bin/env python3
"""name_functions.py -- build-independent __FUNCSIG__/__FILE__ naming pipeline.

A release build ships no symbol table, but MSVC's assert/verify macros bake the
enclosing function's full signature (`__FUNCSIG__`) and source path (`__FILE__`)
into read-only data as string literals, and the only code that loads a given
literal is the function that asserts with it. Following the reference from code
back to the string therefore recovers `function RVA -> real C++ name + source
file` without decompiling. Clang/GCC builds (the Linux/macOS day-one targets)
embed `__PRETTY_FUNCTION__` instead; those are collected too.

This is the `funcsig.py` + `func2src.py` + `src_ranges.py` technique from
`tpf2-multiplayer/tools/re`, made independent of any one build:

  * assert strings are found by scanning read-only data (no hardcoded RVAs);
  * the containing function is resolved from real function bounds -- x64 PE from
    `.pdata` unwind info (with chained-unwind collapse), ELF/Mach-O from LIEF's
    function starts;
  * references are found architecture-aware (x86-64 rip-relative, arm64 ADRP+ADD).

Outputs a symbol map (JSON + CSV), a Ghidra import script, an x64dbg database and
an IDAPython script.

    python tools/re/name_functions.py <binary> -o <outdir> [--prefix tpf2]
    python tools/re/name_functions.py <binary> --validate known_rvas.txt

Validated against Transport Fever 2 build 35924; see investigation/tpf2-baseline/.
"""

from __future__ import annotations

import argparse
import bisect
import collections
import json
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

sys.path.insert(0, str(Path(__file__).resolve().parent))
import tpfbin  # noqa: E402


@dataclass
class NamedFunction:
    rva: int
    size: int
    name: str = ""                       # short qualified name for labels
    name_kind: str = "unknown"           # funcsig|pretty|ambiguous|file-only|inferred|unknown
    signatures: list[str] = field(default_factory=list)
    source_file: str = ""
    source_kind: str = "none"            # direct|inferred|ambiguous|none


# --------------------------------------------------------------------------- #
# Signature -> short name                                                     #
# --------------------------------------------------------------------------- #

_CC = ("__cdecl", "__thiscall", "__stdcall", "__fastcall", "__vectorcall", "__clrcall")


def short_name(signature: str) -> str:
    """Extract the qualified function name from a full signature string.

    `struct Command __cdecl make_cmd::BuildProposal(...)` -> `make_cmd::BuildProposal`.
    `void GameSim::Step(long, int)` (Clang) -> `GameSim::Step`.
    Robust enough for labels; templates/operators keep their punctuation.
    """
    s = signature.strip()
    # drop everything up to and including the calling convention, if present
    for cc in _CC:
        idx = s.find(cc + " ")
        if idx >= 0:
            s = s[idx + len(cc) + 1:]
            break
    # cut at the argument-list open paren (depth 0, ignoring template/type angle)
    depth = 0
    cut = len(s)
    for i, ch in enumerate(s):
        if ch == "<":
            depth += 1
        elif ch == ">":
            depth = max(0, depth - 1)
        elif ch == "(" and depth == 0:
            cut = i
            break
    head = s[:cut].strip()
    # for a Clang pretty-signature the return type is still glued on the front;
    # the name is the last whitespace-separated run (which keeps `A::B::c`).
    if " " in head and "::" not in head.split()[-1] and "::" not in head:
        head = head.split()[-1]
    elif " " in head:
        # keep the trailing token that contains the scope resolution
        parts = head.split()
        head = parts[-1]
    return head or signature


# --------------------------------------------------------------------------- #
# Reference verification (x86-64)                                             #
# --------------------------------------------------------------------------- #

def _verify_x86_refs(image: tpfbin.Image, refs: list[tpfbin.Ref]) -> list[tpfbin.Ref]:
    """Keep only refs that are real rip-relative instruction operands.

    The fast ModR/M scan can match bytes inside data-in-code or mid-instruction.
    Disassembling each containing function once (bounded by .pdata) and checking
    that an instruction really ends at disp+4 rejects those. On TPF2 this drops
    3 of ~88k candidates -- small, but it is the difference between a symbol map
    a hook can trust and one it cannot.
    """
    try:
        from capstone import Cs, CS_ARCH_X86, CS_MODE_64
    except ImportError as exc:  # pragma: no cover
        image.warnings.append("capstone unavailable, skipping x86 ref verification: %s" % exc)
        return refs
    md = Cs(CS_ARCH_X86, CS_MODE_64)
    by_func: dict[int, list[tpfbin.Ref]] = collections.defaultdict(list)
    orphan = []
    for r in refs:
        f = image.containing_function(r.insn_rva)
        if f is None:
            orphan.append(r)
        else:
            by_func[f].append(r)
    kept: list[tpfbin.Ref] = list(orphan)  # refs outside known bounds are kept as-is
    for f, group in by_func.items():
        end = image.function_end(f) or (f + 0x1000)
        blob = image.read_rva(f, end - f)
        if not blob:
            kept.extend(group)
            continue
        ends = {}
        for (addr, size, _mn, _ops) in md.disasm_lite(blob, f):
            ends[addr + size] = addr
        for r in group:
            start = ends.get(r.insn_rva + 4)
            if start is not None and start <= r.insn_rva:
                kept.append(r)
    return kept


# --------------------------------------------------------------------------- #
# Attribution                                                                 #
# --------------------------------------------------------------------------- #

def build_symbol_map(image: tpfbin.Image, syms: tpfbin.SymbolStrings,
                     verify: bool = True) -> tuple[dict[int, NamedFunction], dict]:
    targets = set(syms.funcsig) | set(syms.pretty) | set(syms.files)
    refs = tpfbin.resolve_references(image, targets)
    if verify and image.arch in ("x86_64", "x86"):
        refs = _verify_x86_refs(image, refs)

    file_short, prefix = tpfbin.strip_source_prefix(list(syms.files.values()))

    sig_of: dict[int, set[str]] = collections.defaultdict(set)      # func -> funcsig strings
    pretty_of: dict[int, set[str]] = collections.defaultdict(set)   # func -> pretty strings
    files_of: dict[int, set[str]] = collections.defaultdict(set)    # func -> short source files
    funcs_of_sig: dict[str, set[int]] = collections.defaultdict(set)
    unresolved = 0
    for r in refs:
        f = image.containing_function(r.insn_rva)
        if f is None:
            unresolved += 1
            continue
        if r.target_rva in syms.funcsig:
            sig = syms.funcsig[r.target_rva]
            sig_of[f].add(sig)
            funcs_of_sig[sig].add(f)
        elif r.target_rva in syms.pretty:
            pretty_of[f].add(syms.pretty[r.target_rva])
        elif r.target_rva in syms.files:
            files_of[f].add(file_short.get(syms.files[r.target_rva], syms.files[r.target_rva]))

    functions: dict[int, NamedFunction] = {}

    def ensure(rva: int) -> NamedFunction:
        nf = functions.get(rva)
        if nf is None:
            end = image.function_end(rva) or rva
            nf = NamedFunction(rva=rva, size=max(0, end - rva))
            functions[rva] = nf
        return nf

    # direct signatures
    for f, sigs in sig_of.items():
        nf = ensure(f)
        if len(sigs) == 1:
            chosen = next(iter(sigs))
            nf.name = short_name(chosen)
            nf.name_kind = "funcsig"
        else:
            # several signatures reference the function: inlined copies/wrappers.
            # pick the signature whose own name maps back here as the primary use.
            own = [s for s in sigs if f in funcs_of_sig.get(s, ()) and len(funcs_of_sig[s]) == 1]
            if len(own) == 1:
                chosen = own[0]
                nf.name = short_name(chosen)
                nf.name_kind = "funcsig"
            else:
                chosen = sorted(sigs)[0]
                nf.name = short_name(chosen)
                nf.name_kind = "ambiguous"
        # keep the chosen signature first so CSV/comments match the recovered name
        nf.signatures = [chosen] + sorted(s for s in sigs if s != chosen)
    # pretty signatures where no MSVC signature was found
    for f, prettys in pretty_of.items():
        nf = ensure(f)
        if nf.name_kind in ("funcsig", "ambiguous"):
            continue
        nf.signatures = sorted(prettys)
        nf.name = short_name(sorted(prettys)[0])
        nf.name_kind = "pretty" if len(prettys) == 1 else "ambiguous"

    # direct file attribution
    for f, fs in files_of.items():
        nf = ensure(f)
        if len(fs) == 1:
            nf.source_file = next(iter(fs))
            nf.source_kind = "direct"
        else:
            nf.source_file = sorted(fs)[0]
            nf.source_kind = "ambiguous"

    # translation-unit range inference: the linker keeps one .cpp contiguous, so
    # the first and last directly-attributed function of a file bracket it, and
    # unnamed functions inside the run belong to the same file (src_ranges.py).
    anchors = sorted((f, next(iter(fs))) for f, fs in files_of.items() if len(fs) == 1)
    runs: list[tuple[str, int, int]] = []
    i = 0
    while i < len(anchors):
        fstart, fname = anchors[i]
        j = i
        last = fstart
        while j + 1 < len(anchors) and anchors[j + 1][1] == fname:
            j += 1
            last = anchors[j][0]
        runs.append((fname, fstart, last))
        i = j + 1
    begins = image.function_starts
    for fname, lo, hi in runs:
        if lo == hi:
            continue  # a single anchor is a point, not evidence of extent
        a = bisect.bisect_left(begins, lo)
        b = bisect.bisect_right(begins, hi)
        for rva in begins[a:b]:
            nf = ensure(rva)
            if nf.source_kind == "none":
                nf.source_file = fname
                nf.source_kind = "inferred"
            if nf.name_kind == "unknown":
                nf.name_kind = "file-only"

    stats = {
        "image_arch": image.arch,
        "image_format": image.fmt,
        "total_functions": len(image.function_starts),
        "assert_funcsig_strings": len(syms.funcsig),
        "assert_pretty_strings": len(syms.pretty),
        "assert_file_strings": len(syms.files),
        "references_resolved": len(refs),
        "references_outside_bounds": unresolved,
        "functions_named_funcsig": sum(1 for n in functions.values() if n.name_kind == "funcsig"),
        "functions_named_pretty": sum(1 for n in functions.values() if n.name_kind == "pretty"),
        "functions_named_ambiguous": sum(1 for n in functions.values() if n.name_kind == "ambiguous"),
        "functions_file_direct": sum(1 for n in functions.values() if n.source_kind == "direct"),
        "functions_file_inferred": sum(1 for n in functions.values() if n.source_kind == "inferred"),
        "distinct_source_files": len({n.source_file for n in functions.values() if n.source_file}),
        "source_prefix": prefix,
    }
    return functions, stats


# --------------------------------------------------------------------------- #
# Output writers                                                              #
# --------------------------------------------------------------------------- #

def write_json(path: Path, image: tpfbin.Image, functions: dict[int, NamedFunction], stats: dict) -> None:
    doc = {
        "binary": image.path.name,
        "format": image.fmt,
        "arch": image.arch,
        "image_base": image.image_base,
        "stats": stats,
        "functions": [
            {"rva": nf.rva, "va": image.image_base + nf.rva, "size": nf.size,
             "name": nf.name, "name_kind": nf.name_kind,
             "source_file": nf.source_file, "source_kind": nf.source_kind,
             "signatures": nf.signatures}
            for nf in sorted(functions.values(), key=lambda n: n.rva)
        ],
    }
    path.write_text(json.dumps(doc, indent=1, sort_keys=False), encoding="utf-8")


def write_csv(path: Path, image: tpfbin.Image, functions: dict[int, NamedFunction]) -> None:
    import csv
    with path.open("w", newline="", encoding="utf-8") as fh:
        w = csv.writer(fh)
        w.writerow(["rva", "va", "size", "name", "name_kind", "n_signatures",
                    "source_file", "source_kind", "signature"])
        for nf in sorted(functions.values(), key=lambda n: n.rva):
            w.writerow(["%x" % nf.rva, "%x" % (image.image_base + nf.rva), nf.size,
                        nf.name, nf.name_kind, len(nf.signatures),
                        nf.source_file, nf.source_kind,
                        nf.signatures[0] if nf.signatures else ""])


def write_ghidra(path: Path, image: tpfbin.Image, functions: dict[int, NamedFunction]) -> None:
    """A self-contained Ghidra Python (Jython) script that applies the map."""
    named = [nf for nf in sorted(functions.values(), key=lambda n: n.rva)
             if nf.name or nf.source_file]
    lines = [
        "# Auto-generated by name_functions.py -- apply recovered names to a Ghidra program.",
        "# Run under Ghidra (Script Manager or analyzeHeadless). Image base is read from",
        "# the program, so it works whether or not the disk RVAs match the loaded base.",
        "#@category TPF3MP",
        "base = currentProgram.getImageBase()",
        "fm = currentProgram.getFunctionManager()",
        "st = currentProgram.getSymbolTable()",
        "from ghidra.program.model.symbol import SourceType",
        "def apply(rva, name, sig, src):",
        "    addr = base.add(rva)",
        "    fn = fm.getFunctionContaining(addr)",
        "    if fn is None:",
        "        try: fn = createFunction(addr, None)",
        "        except: fn = None",
        "    if name:",
        "        try:",
        "            if fn is not None: fn.setName(name, SourceType.USER_DEFINED)",
        "            else: createLabel(addr, name, True)",
        "        except: pass",
        "    cmt = sig or ''",
        "    if src: cmt = (cmt + '\\n' if cmt else '') + 'source: ' + src",
        "    if cmt:",
        "        try: setPlateComment(addr, cmt)",
        "        except: pass",
        "DATA = [",
    ]
    for nf in named:
        sig = nf.signatures[0].replace("\\", "\\\\").replace("'", "\\'") if nf.signatures else ""
        nm = nf.name.replace("\\", "\\\\").replace("'", "\\'")
        src = nf.source_file.replace("\\", "\\\\").replace("'", "\\'")
        lines.append("  (0x%x, '%s', '%s', '%s')," % (nf.rva, nm, sig, src))
    lines.append("]")
    lines.append("for rva, name, sig, src in DATA: apply(rva, name, sig, src)")
    lines.append("print('applied %d names' % len(DATA))")
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def write_x64dbg(path: Path, image: tpfbin.Image, functions: dict[int, NamedFunction]) -> None:
    """x64dbg database (.dd64/.json): module-relative labels and comments."""
    module = image.path.name.lower()
    labels, comments = [], []
    for nf in sorted(functions.values(), key=lambda n: n.rva):
        if nf.name:
            labels.append({"module": module, "address": "0x%X" % nf.rva,
                           "manual": True, "text": nf.name})
        note = nf.signatures[0] if nf.signatures else ""
        if nf.source_file:
            note = (note + " | " if note else "") + "src:" + nf.source_file
        if note:
            comments.append({"module": module, "address": "0x%X" % nf.rva,
                             "manual": True, "text": note})
    path.write_text(json.dumps({"labels": labels, "comments": comments}, indent=1),
                    encoding="utf-8")


def write_ida(path: Path, image: tpfbin.Image, functions: dict[int, NamedFunction]) -> None:
    """IDAPython script: set_name + set_cmt relative to the loaded image base."""
    lines = [
        "# Auto-generated by name_functions.py -- apply recovered names in IDA.",
        "# File > Script file... (IDAPython). Addresses are rebased to the current image base.",
        "import ida_name, ida_bytes, ida_funcs, idaapi",
        "base = idaapi.get_imagebase()",
        "DISK_BASE = 0x%x" % image.image_base,
        "def apply(rva, name, cmt):",
        "    ea = base + rva",
        "    if name:",
        "        ida_name.set_name(ea, name, ida_name.SN_FORCE | ida_name.SN_NOCHECK)",
        "    if cmt:",
        "        ida_bytes.set_cmt(ea, cmt, 1)",
        "DATA = [",
    ]
    for nf in sorted(functions.values(), key=lambda n: n.rva):
        if not (nf.name or nf.source_file):
            continue
        cmt = nf.signatures[0] if nf.signatures else ""
        if nf.source_file:
            cmt = (cmt + " | " if cmt else "") + "src:" + nf.source_file
        nm = nf.name.replace("\\", "\\\\").replace('"', '\\"')
        cmt = cmt.replace("\\", "\\\\").replace('"', '\\"')
        lines.append('  (0x%x, "%s", "%s"),' % (nf.rva, nm, cmt))
    lines.append("]")
    lines.append("for rva, name, cmt in DATA: apply(rva, name, cmt)")
    lines.append('print("applied %d names" % len(DATA))')
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


# --------------------------------------------------------------------------- #
# Validation                                                                  #
# --------------------------------------------------------------------------- #

def run_validation(functions: dict[int, NamedFunction], spec_path: Path,
                   image: tpfbin.Image) -> tuple[bool, list[str]]:
    """Check the recovered map against known RVAs, in two strict modes.

    Spec lines ('#' comments):
      <rva> <name-substring>     the binary DOES assert with this function's
                                 signature, so the pipeline MUST name it and the
                                 substring must appear in the name (or signature).
      <rva> ~ <src-substring>    the binary does NOT embed this function's
                                 __FUNCSIG__ (e.g. TPF2's make_cmd::BuildProposal),
                                 so the pipeline must NOT invent a name, and must
                                 instead attribute it to a source file whose path
                                 contains <src-substring>.
    Both directions can fail, so a broken pipeline that stops naming, or that
    starts inventing names, is caught.
    """
    out: list[str] = []
    ok = True
    for raw in spec_path.read_text(encoding="utf-8").splitlines():
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        parts = line.split(None, 1)
        rva = int(parts[0], 16)  # RVAs in the spec are always hex
        rest = parts[1].strip() if len(parts) > 1 else ""
        f = image.containing_function(rva)
        nf = functions.get(rva) or (functions.get(f) if f is not None else None)
        name = nf.name if nf else ""
        sig = (nf.signatures[0] if nf and nf.signatures else "")
        src = nf.source_file if nf else ""
        kind = nf.name_kind if nf else "absent"

        if rest.startswith("~"):  # must be declined and attributed to a file
            want_src = rest[1:].strip().lower()
            if name:
                ok = False
                out.append("  0x%-8x  UNEXPECTED NAME %r (binary embeds no __FUNCSIG__ here)" % (rva, name))
            elif want_src and want_src not in src.lower():
                ok = False
                out.append("  0x%-8x  WRONG SOURCE got %r ; expected ~ %r" % (rva, src, want_src))
            else:
                out.append("  0x%-8x  OK  not funcsig-named; source=%s" % (rva, src or "-"))
            continue

        # must be named
        if not name:
            ok = False
            out.append("  0x%-8x  NOT NAMED (expected ~ %r; source=%s)" % (rva, rest, src or "-"))
        elif rest and rest.lower() not in name.lower() and rest.lower() not in sig.lower():
            ok = False
            out.append("  0x%-8x  MISMATCH got %r [%s] ; expected ~ %r" % (rva, name, kind, rest))
        else:
            out.append("  0x%-8x  OK  %s  [%s]  src=%s" % (rva, name, kind, src or "-"))
    return ok, out


# --------------------------------------------------------------------------- #
# CLI                                                                         #
# --------------------------------------------------------------------------- #

def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(
        description="Recover a partial symbol map from a build's assert strings (PE/ELF/Mach-O).")
    ap.add_argument("binary", type=Path)
    ap.add_argument("-o", "--outdir", type=Path,
                    help="write symbols.{json,csv}, the Ghidra/x64dbg/IDA scripts here")
    ap.add_argument("--prefix", default=None,
                    help="basename stem for the output files (default: the binary stem)")
    ap.add_argument("--no-verify", action="store_true",
                    help="skip per-function capstone verification of x86 references (faster, less precise)")
    ap.add_argument("--validate", type=Path,
                    help="a '<rva> <expected-name>' spec to check the map against (exit 3 on mismatch)")
    ap.add_argument("--quiet", action="store_true", help="only print the coverage summary")
    args = ap.parse_args(argv)

    if not args.binary.is_file():
        ap.error("no such file: %s" % args.binary)
    try:
        image = tpfbin.Image.load(args.binary)
    except Exception as exc:
        print("name_functions: cannot analyse %s: %s" % (args.binary, exc), file=sys.stderr)
        return 2

    syms = tpfbin.extract_symbol_strings(image)
    functions, stats = build_symbol_map(image, syms, verify=not args.no_verify)

    print("== %s (%s, %s)" % (image.path.name, image.fmt.upper(), image.arch))
    for k, v in stats.items():
        print("   %-28s %s" % (k, v))
    for wn in image.warnings:
        print("   ! warning: %s" % wn)

    if args.outdir:
        args.outdir.mkdir(parents=True, exist_ok=True)
        stem = args.prefix or image.path.stem
        write_json(args.outdir / (stem + ".symbols.json"), image, functions, stats)
        write_csv(args.outdir / (stem + ".symbols.csv"), image, functions)
        write_ghidra(args.outdir / (stem + ".ghidra.py"), image, functions)
        write_x64dbg(args.outdir / (stem + ".x64dbg.json"), image, functions)
        write_ida(args.outdir / (stem + ".ida.py"), image, functions)
        print("   wrote %s.{symbols.json,symbols.csv,ghidra.py,x64dbg.json,ida.py} -> %s"
              % (stem, args.outdir))

    rc = 0
    if args.validate:
        ok, lines = run_validation(functions, args.validate, image)
        print("\n== validation (%s)" % args.validate)
        for ln in lines:
            print(ln)
        print("   %s" % ("ALL OK" if ok else "MISMATCHES PRESENT"))
        if not ok:
            rc = 3
    return rc


if __name__ == "__main__":
    raise SystemExit(main())
