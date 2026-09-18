#!/usr/bin/env python3
"""diff_builds.py -- compare two symbol maps so hook signatures can be re-verified.

TPF3 is expected to patch often after launch (docs/ARCHITECTURE.md: "TPF3 will
be patched often"). Each patch moves functions, and every byte-signature hook is
pinned to a build. Given the `name_functions.py` symbol maps of two builds (say
the day-one build and a day-two patch), this reports, per named function, which
ones:

  * moved   -- same name and size, different RVA (re-derive the address);
  * resized -- same name, different size (body changed; re-verify prologue bytes
               and signature -- these are the risky ones for a hook);
  * appeared / disappeared -- a signature the newer/older build no longer has
               (re-target, or a hook lost its anchor).

Functions are matched by their recovered name plus source file (stable across
builds), not by address (which always changes). A global uniform shift is normal
and is summarised rather than listed line by line.

    python tools/re/diff_builds.py OLD.symbols.json NEW.symbols.json [-o report.md]
"""

from __future__ import annotations

import argparse
import collections
import json
import sys
from pathlib import Path
from typing import Optional


def _load(path: Path) -> dict:
    doc = json.loads(path.read_text(encoding="utf-8"))
    if "functions" not in doc:
        raise ValueError("%s is not a name_functions symbol map (no 'functions')" % path)
    return doc


def _key(fn: dict) -> str:
    """Stable cross-build identity: recovered name + source file."""
    name = fn.get("name") or ""
    src = fn.get("source_file") or ""
    return name + "\x1f" + src


def _index(doc: dict) -> "collections.OrderedDict[str, list[dict]]":
    """name+src -> functions with that identity, sorted by RVA (for positional match)."""
    groups: dict[str, list[dict]] = collections.defaultdict(list)
    for fn in doc["functions"]:
        if not fn.get("name"):
            continue  # only named functions have a cross-build identity
        groups[_key(fn)].append(fn)
    for v in groups.values():
        v.sort(key=lambda f: f["rva"])
    return collections.OrderedDict(sorted(groups.items()))


def diff(old: dict, new: dict) -> dict:
    oidx, nidx = _index(old), _index(new)
    moved, resized, appeared, disappeared, unchanged = [], [], [], [], 0
    for key in sorted(set(oidx) | set(nidx)):
        og, ng = oidx.get(key, []), nidx.get(key, [])
        name, src = key.split("\x1f", 1)
        # match positionally within a same-identity group
        for i in range(min(len(og), len(ng))):
            o, n = og[i], ng[i]
            if o["size"] != n["size"]:
                resized.append((name, src, o["rva"], n["rva"], o["size"], n["size"]))
            elif o["rva"] != n["rva"]:
                moved.append((name, src, o["rva"], n["rva"]))
            else:
                unchanged += 1
        for o in og[len(ng):]:
            disappeared.append((name, src, o["rva"], o["size"]))
        for n in ng[len(og):]:
            appeared.append((name, src, n["rva"], n["size"]))
    # a build-wide constant RVA shift is expected; measure it so it can be discounted
    deltas = collections.Counter(n - o for _, _, o, n in moved)
    common_shift = deltas.most_common(1)[0] if deltas else (0, 0)
    return {"moved": moved, "resized": resized, "appeared": appeared,
            "disappeared": disappeared, "unchanged": unchanged,
            "common_shift": common_shift}


def build_report(old_doc: dict, new_doc: dict, old_path: Path, new_path: Path) -> str:
    d = diff(old_doc, new_doc)
    L: list[str] = []
    w = L.append
    w("# Build diff: `%s` -> `%s`" % (old_doc.get("binary", old_path.name),
                                      new_doc.get("binary", new_path.name)))
    w("")
    w("Produced by `tools/re/diff_builds.py`. Functions are matched by recovered "
      "name + source file across the two symbol maps.")
    w("")
    w("| | old | new |")
    w("|---|---|---|")
    w("| binary | `%s` | `%s` |" % (old_doc.get("binary", "?"), new_doc.get("binary", "?")))
    w("| arch | %s | %s |" % (old_doc.get("arch", "?"), new_doc.get("arch", "?")))
    w("| named functions | %d | %d |" % (
        sum(1 for f in old_doc["functions"] if f.get("name")),
        sum(1 for f in new_doc["functions"] if f.get("name"))))
    w("")
    shift, shift_n = d["common_shift"]
    w("## Summary")
    w("")
    w("- unchanged (same name, size, RVA): **%d**" % d["unchanged"])
    w("- moved (same size, new RVA): **%d**" % len(d["moved"]))
    w("- resized (body changed -- re-verify signatures): **%d**" % len(d["resized"]))
    w("- appeared (new in newer build): **%d**" % len(d["appeared"]))
    w("- disappeared (gone from newer build): **%d**" % len(d["disappeared"]))
    if shift_n:
        w("- most common RVA delta among moved functions: 0x%X (%d functions) -- "
          "a uniform shift of this size is an ordinary relayout." % (shift & 0xFFFFFFFF, shift_n))
    w("")

    w("## Resized functions (re-verify hook signatures)")
    w("")
    if d["resized"]:
        w("| function | source | old RVA | new RVA | old size | new size |")
        w("|---|---|---|---|---|---|")
        for name, src, orva, nrva, osz, nsz in d["resized"][:3000]:
            w("| `%s` | %s | 0x%X | 0x%X | %d | %d |" % (name, src, orva, nrva, osz, nsz))
        if len(d["resized"]) > 3000:
            w("")
            w("_... %d more._" % (len(d["resized"]) - 3000))
    else:
        w("_None._")
    w("")

    w("## Appeared (new signatures in the newer build)")
    w("")
    if d["appeared"]:
        w("| function | source | RVA | size |")
        w("|---|---|---|---|")
        for name, src, rva, sz in d["appeared"][:3000]:
            w("| `%s` | %s | 0x%X | %d |" % (name, src, rva, sz))
        if len(d["appeared"]) > 3000:
            w("")
            w("_... %d more._" % (len(d["appeared"]) - 3000))
    else:
        w("_None._")
    w("")

    w("## Disappeared (gone from the newer build)")
    w("")
    if d["disappeared"]:
        w("| function | source | RVA | size |")
        w("|---|---|---|---|")
        for name, src, rva, sz in d["disappeared"][:3000]:
            w("| `%s` | %s | 0x%X | %d |" % (name, src, rva, sz))
        if len(d["disappeared"]) > 3000:
            w("")
            w("_... %d more._" % (len(d["disappeared"]) - 3000))
    else:
        w("_None._")
    w("")

    w("## Moved only (same size, new address)")
    w("")
    w("%d functions moved with an unchanged body. A hook re-derives their address "
      "from the symbol map; the byte signature should still match." % len(d["moved"]))
    w("")
    return "\n".join(L) + "\n"


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(
        description="Diff two name_functions symbol maps (JSON) to re-verify hooks after a patch.")
    ap.add_argument("old", type=Path, help="older build's <name>.symbols.json")
    ap.add_argument("new", type=Path, help="newer build's <name>.symbols.json")
    ap.add_argument("-o", "--output", type=Path, help="write Markdown here (default: stdout)")
    args = ap.parse_args(argv)

    for p in (args.old, args.new):
        if not p.is_file():
            ap.error("no such file: %s" % p)
    try:
        old_doc, new_doc = _load(args.old), _load(args.new)
    except Exception as exc:
        print("diff_builds: %s" % exc, file=sys.stderr)
        return 2
    report = build_report(old_doc, new_doc, args.old, args.new)
    if args.output:
        args.output.write_text(report, encoding="utf-8")
        print("wrote %s" % args.output, file=sys.stderr)
    else:
        sys.stdout.write(report)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
