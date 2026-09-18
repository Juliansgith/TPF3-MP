#!/usr/bin/env python3
"""binary_survey.py -- one-command static survey of a Transport Fever binary.

Writes a Markdown report answering the release-day "static recon" checklist in
`docs/DAY_ONE.md` section 2 for a PE (Windows), ELF (Linux) or Mach-O (macOS)
build:

  identity (hashes, timestamps, sizes, architecture); sections with entropy;
  imports split into system vs game-folder libraries with proxy-loader ranking;
  exports; TLS callbacks; packer / anti-tamper indicators; RTTI presence;
  embedded Lua version strings; __FUNCSIG__ / __FILE__ string counts;
  TPF2-era engine symbol names; and, for Mach-O, code-signing posture.

Read-only: the target is never launched or modified. Deterministic: the report
depends only on the input bytes (no wall-clock time), so two runs on one build
diff cleanly.

    python tools/re/binary_survey.py <binary> [-o report.md]

Validated against Transport Fever 2 build 35924; see investigation/tpf2-baseline/.
"""

from __future__ import annotations

import argparse
import hashlib
import re
import struct
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import tpfbin  # noqa: E402

# Windows system libraries that are never proxy candidates (shipped by the OS).
_SYSTEM_HINTS = ("api-ms-win", "ext-ms-win", "kernel32", "user32", "gdi32",
                 "advapi32", "shell32", "ole32", "oleaut32", "ws2_32", "winhttp",
                 "psapi", "version", "setupapi", "winmm", "imm32", "dbghelp",
                 "opengl32", "vcruntime", "msvcp", "concrt", "ucrtbase", "ntdll",
                 "crypt32", "bcrypt", "secur32", "wldap32", "iphlpapi", "dnsapi")
_MACHO_SYSTEM = ("/usr/lib/", "/system/library/")

# Known protector fingerprints: section names and string markers.
_PACKER_SECTIONS = {
    ".vmp0": "VMProtect", ".vmp1": "VMProtect", ".vmp2": "VMProtect",
    ".themida": "Themida/WinLicense", ".winlic": "Themida/WinLicense",
    ".enigma1": "Enigma", ".enigma2": "Enigma",
    "upx0": "UPX", "upx1": "UPX", ".aspack": "ASPack", ".adata": "ASPack",
    ".bind": "Steam DRM (SteamStub)", ".arch": "possible SteamStub",
    ".petite": "Petite", "_RDATA": None,
}
_PACKER_STRINGS = {
    "Denuvo": "Denuvo Anti-Tamper", ".vmp": "VMProtect", "VMProtect": "VMProtect",
    "Themida": "Themida", "WinLicense": "WinLicense", "SteamStub": "SteamStub",
    "Enigma": "Enigma", "EASharedMemoryFrame": "EA/Denuvo",
    "EasyAntiCheat": "EasyAntiCheat", "BattlEye": "BattlEye", "Arxan": "Arxan/GuardIT",
    "SecuROM": "SecuROM", "StarForce": "StarForce",
}


def _human(n: int) -> str:
    if n < 1024:
        return "%d B" % n
    v = float(n)
    for unit in ("KiB", "MiB", "GiB", "TiB"):
        v /= 1024.0
        if v < 1024 or unit == "TiB":
            return "%.2f %s (%d B)" % (v, unit, n)
    return "%d B" % n


def _steamstub_header(img: tpfbin.Image) -> dict | None:
    """Best-effort decode of the SteamStub header that precedes the entry point.

    Only meaningful for a PE with a `.bind` section. The header sits immediately
    before AddressOfEntryPoint and is XOR-chained under its own first dword. The
    field layout below is the well-documented SteamStub v3.x x64 variant; fields
    that do not decode are simply omitted.
    """
    if img.fmt != "pe":
        return None
    if not any(s.name.lower() == ".bind" for s in img.sections):
        return None
    ep_off = img.pe.get_offset_from_rva(img.entrypoint_rva)
    for size in (0xF0, 0xD0, 0xB0):
        if ep_off - size < 0:
            continue
        hdr = bytearray(img.data[ep_off - size:ep_off])
        key = struct.unpack_from("<I", hdr, 0)[0]
        out = bytearray(hdr)
        for x in range(4, size, 4):
            val = struct.unpack_from("<I", hdr, x)[0]
            struct.pack_into("<I", out, x, val ^ key)
            key = val
        sig = struct.unpack_from("<I", out, 4)[0]
        if sig in (0xC0DEC0DF, 0xC0DEC0DE):
            # SteamStub v3.x x64 header layout (offsets into the decrypted block):
            #   +0x00 XorKey  +0x04 Signature  +0x08 ImageBase  +0x10 EntryPoint
            #   +0x18 BindOff +0x20 OriginalEntryPoint  +0x38 SteamAppId  +0x3C Flags
            oep = struct.unpack_from("<Q", out, 0x20)[0]
            appid = struct.unpack_from("<I", out, 0x38)[0] if size >= 0x40 else None
            flags = struct.unpack_from("<I", out, 0x3C)[0] if size >= 0x40 else None
            info = {"variant": "v3.x (header 0x%X)" % size, "signature": "0x%08X" % sig,
                    "original_entry_point_rva": "0x%X" % oep}
            if appid is not None:
                info["steam_app_id"] = appid
            if flags is not None:
                info["flags"] = "0x%X" % flags
            return info
    return {"variant": "unknown", "note": "header did not decode with known layouts"}


def _scan_flags(data: bytes) -> dict:
    strings = data  # raw bytes; substring search is enough for markers
    out = {}
    for marker, label in _PACKER_STRINGS.items():
        c = strings.count(marker.encode("ascii"))
        if c:
            out[label] = out.get(label, 0) + c
    return out


def _lua_versions(data: bytes) -> list[str]:
    found = []
    for m in re.finditer(rb"Lua ?5\.[0-9](?:\.[0-9])?", data):
        s = m.group().decode("ascii")
        if s not in found:
            found.append(s)
    for m in re.finditer(rb"\$Lua(?:Version|Authors)[^$]{0,120}\$", data):
        s = m.group().decode("ascii", "replace")
        if s not in found:
            found.append(s)
    for m in re.finditer(rb"LuaJIT \d\.\d\.\d+", data):
        s = m.group().decode("ascii")
        if s not in found:
            found.append(s)
    return found


def _rtti_count(data: bytes) -> tuple[int, int, list[str]]:
    msvc = re.findall(rb"\.\?A[UV][\w@?$]+@@", data)
    itanium = re.findall(rb"_ZTS[\w]+", data)  # Itanium type-info name symbols
    samples = []
    for m in msvc[:6]:
        samples.append(m.decode("ascii", "replace"))
    for m in itanium[:6]:
        samples.append(m.decode("ascii", "replace"))
    return len(msvc), len(itanium), samples


def _export_count(path: Path) -> tuple[int, int]:
    """(#exports, #forwarders) for a sibling PE/ELF/Mach-O, or (-1, -1) on failure."""
    try:
        img = tpfbin.Image.load(path, image_dir_names=set())
        fwd = 0
        return len(img.exports), fwd
    except Exception:
        return -1, -1


def build_report(img: tpfbin.Image) -> str:
    data = img.data
    L: list[str] = []
    w = L.append
    w("# Binary survey: `%s`" % img.path.name)
    w("")
    w("Static, read-only survey produced by `tools/re/binary_survey.py`. "
      "The target was not launched or modified.")
    w("")

    # -- identity ------------------------------------------------------------ #
    w("## 1. Identity")
    w("")
    w("| field | value |")
    w("|---|---|")
    w("| path | `%s` |" % img.path)
    w("| format | %s |" % img.fmt.upper())
    w("| architecture | %s (%d-bit, %s-endian) |" % (img.arch, img.bits, img.endianness))
    w("| file size | %s |" % _human(len(data)))
    w("| SHA-256 | `%s` |" % hashlib.sha256(data).hexdigest())
    w("| SHA-1 | `%s` |" % hashlib.sha1(data).hexdigest())
    w("| MD5 | `%s` |" % hashlib.md5(data).hexdigest())
    w("| image base | 0x%X |" % img.image_base)
    w("| entry point | RVA 0x%X (VA 0x%X) |" % (img.entrypoint_rva, img.image_base + img.entrypoint_rva))
    if img.fmt == "pe":
        ts = img.extra.get("timestamp", 0)
        w("| PE timestamp | 0x%08X |" % ts)
        w("| linker version | %s |" % img.extra.get("linker", "?"))
        w("| SizeOfImage | 0x%X |" % img.extra.get("image_size", 0))
        dc = img.extra.get("dll_characteristics", 0)
        flags = []
        for bit, name in ((0x20, "HIGH_ENTROPY_VA"), (0x40, "DYNAMIC_BASE/ASLR"),
                          (0x100, "NX_COMPAT/DEP"), (0x400, "NO_SEH"),
                          (0x4000, "CONTROL_FLOW_GUARD")):
            if dc & bit:
                flags.append(name)
        w("| DLL characteristics | 0x%04X %s |" % (dc, ", ".join(flags)))
        if "pdb" in img.extra:
            w("| PDB path | `%s` |" % img.extra["pdb"])
        w("| Authenticode signed | %s |" % ("yes" if img.extra.get("authenticode") else "no"))
        ov = img.extra.get("overlay_size", 0)
        w("| overlay | %s |" % (_human(ov) if ov else "none"))
    if img.fmt == "elf":
        if "interpreter" in img.extra:
            w("| interpreter | `%s` |" % img.extra["interpreter"])
        if "build_id" in img.extra:
            w("| GNU build-id | `%s` |" % img.extra["build_id"])
        w("| PIE | %s |" % img.extra.get("is_pie"))
        w("| NX | %s |" % img.extra.get("nx"))
    w("")
    if "rich" in img.extra:
        w("Rich header (MSVC toolchain components, `prodid build count`):")
        w("")
        w("```")
        for prodid, build, count in img.extra["rich"]:
            w("prodid=%d build=%d count=%d" % (prodid, build, count))
        w("```")
        w("")

    # -- sections ------------------------------------------------------------ #
    w("## 2. Sections")
    w("")
    w("Entropy near 8.0 in an executable or data section is a packing/encryption "
      "signal. RWX or high-entropy executable sections are called out below.")
    w("")
    w("| name | RVA | virtual size | raw size | perms | entropy |")
    w("|---|---|---|---|---|---|")
    for s in img.sections:
        perms = "%s%s%s" % ("r" if s.readable else "-", "w" if s.writable else "-",
                            "x" if s.executable else "-")
        w("| `%s` | 0x%X | 0x%X | 0x%X | %s | %.3f |" % (
            s.name, s.vaddr, s.vsize, s.file_size, perms, s.entropy))
    w("")
    suspicious = [s for s in img.sections
                  if (s.executable and s.entropy > 7.2 and s.file_size)
                  or (s.writable and s.executable)]
    if suspicious:
        w("> Note: " + "; ".join(
            "`%s` %s%s" % (s.name,
                           "entropy %.2f" % s.entropy if s.entropy > 7.2 else "",
                           " RWX" if (s.writable and s.executable) else "")
            for s in suspicious))
        w("")

    # -- imports ------------------------------------------------------------- #
    w("## 3. Imports")
    w("")
    sys_libs, game_libs = [], []
    for lib in img.imports:
        low = lib.name.lower()
        is_system = any(h in low for h in _SYSTEM_HINTS) or any(h in low for h in _MACHO_SYSTEM)
        if lib.from_image_dir and not is_system:
            game_libs.append(lib)
        elif lib.from_image_dir:
            game_libs.append(lib)
        else:
            sys_libs.append(lib)
    w("Game-folder libraries (imported from the image's own directory) are the "
      "ones a proxy loader can stand in front of. System libraries are shipped "
      "by the OS.")
    w("")
    w("### Game-folder libraries")
    w("")
    if game_libs:
        w("| library | imported symbols | exports | proxy score |")
        w("|---|---|---|---|")
        ranked = []
        for lib in game_libs:
            sib = img.path.parent / lib.name.split(" ")[0]
            nexp, _ = _export_count(sib) if sib.exists() else (-1, -1)
            ranked.append((lib, nexp))
        # proxy candidate: few exports first, statically imported preferred
        def score(item):
            lib, nexp = item
            delay = "(delay)" in lib.name
            return (delay, nexp if nexp >= 0 else 1 << 30)
        ranked.sort(key=score)
        for rank, (lib, nexp) in enumerate(ranked, 1):
            note = "**candidate**" if (rank == 1 and nexp >= 0 and "(delay)" not in lib.name) else ""
            w("| `%s` | %d | %s | %s |" % (
                lib.name, len(lib.symbols),
                str(nexp) if nexp >= 0 else "?", note))
        w("")
        best = ranked[0][0] if ranked else None
        if best and "(delay)" not in best.name and ranked[0][1] >= 0:
            w("Best proxy-loader candidate: **`%s`** -- statically imported from the "
              "game folder with the fewest exports (%d). This is the `alut.dll` role "
              "in the TPF2 mods." % (best.name, ranked[0][1]))
            w("")
    else:
        w("_None found._ Without a game-folder DLL to proxy, the Windows loader "
          "plan (proxy DLL) needs another injection route; see docs/DAY_ONE.md section 5.")
        w("")
    w("### System libraries (%d)" % len(sys_libs))
    w("")
    for lib in sorted(sys_libs, key=lambda x: x.name.lower()):
        shown = lib.symbols[:8]
        more = "" if len(lib.symbols) <= 8 else " ... +%d" % (len(lib.symbols) - 8)
        w("- `%s` (%d): %s%s" % (lib.name, len(lib.symbols), ", ".join(shown), more))
    w("")

    # -- exports ------------------------------------------------------------- #
    w("## 4. Exports")
    w("")
    if img.exports:
        w("%d exported symbols. First 40:" % len(img.exports))
        w("")
        w("```")
        for name, rva in sorted(img.exports)[:40]:
            w("0x%08X  %s" % (rva, name))
        w("```")
        w("")
        w("The export set names the statically-linked libraries folded into the "
          "image (e.g. FreeType `FT_*` here), which is a fingerprint of the build.")
    else:
        w("_No exports._")
    w("")

    # -- TLS / initializers -------------------------------------------------- #
    w("## 5. %s" % img.tls_label)
    w("")
    if img.tls_callbacks:
        w("Code that runs before the entry point (a proxy DLL must expect it):")
        w("")
        for cb in img.tls_callbacks:
            w("- RVA 0x%X (VA 0x%X)" % (cb, img.image_base + cb))
    else:
        w("_None._")
    w("")

    # -- packer / anti-tamper ------------------------------------------------ #
    w("## 6. Packer and anti-tamper indicators")
    w("")
    hits = []
    for s in img.sections:
        label = _PACKER_SECTIONS.get(s.name) or _PACKER_SECTIONS.get(s.name.lower())
        if label:
            hits.append("section `%s` -> %s" % (s.name, label))
    string_hits = _scan_flags(data)
    ss = _steamstub_header(img)
    if hits:
        w("Section-name matches:")
        w("")
        for h in hits:
            w("- %s" % h)
        w("")
    if string_hits:
        w("String markers:")
        w("")
        for label, count in sorted(string_hits.items()):
            w("- %s (%d occurrence(s))" % (label, count))
        w("")
    if ss:
        w("SteamStub (`.bind`) header decode:")
        w("")
        w("```")
        for k, v in ss.items():
            w("%-26s %s" % (k, v))
        w("```")
        w("")
        w("> SteamStub decrypts the real code at load time behind the entry point. "
          "A static tool sees only the stub, so signature scanning of the game's "
          "own functions must run against the in-memory image (or a Steamless-style "
          "dumped image), not the on-disk file. This is the TPF2 situation and does "
          "not by itself block a proxy-DLL hook, which loads into the unpacked process.")
        w("")
    if not (hits or string_hits or ss):
        w("No packer/anti-tamper section names or string markers found. The "
          "on-disk code appears unpacked.")
        w("")

    # -- RTTI ---------------------------------------------------------------- #
    w("## 7. RTTI")
    w("")
    msvc, itanium, samples = _rtti_count(data)
    if msvc or itanium:
        w("- MSVC RTTI type descriptors (`.?AV`/`.?AU...@@`): **%d**" % msvc)
        w("- Itanium type-info names (`_ZTS...`): **%d**" % itanium)
        w("")
        w("Present RTTI gives an independent naming axis (vftables -> class + slot), "
          "as in the TPF2 pipeline. Samples:")
        w("")
        w("```")
        for s in samples:
            w(s)
        w("```")
    else:
        w("No RTTI type descriptors found (unusual for a C++ game; may indicate "
          "`/GR-` or a stripped build).")
    w("")

    # -- Lua ----------------------------------------------------------------- #
    w("## 8. Embedded Lua")
    w("")
    luas = _lua_versions(data)
    if luas:
        for s in luas:
            w("- `%s`" % s)
        w("")
        w("The Lua version fixes the script sandbox the probe mods assume "
          "(docs/DAY_ONE.md section 3). TPF2 is Lua 5.2.2 with sol2 bindings.")
    else:
        w("_No Lua version string found._ The probe mods still detect `_VERSION` "
          "at runtime.")
    w("")

    # -- assert strings ------------------------------------------------------ #
    w("## 9. Compiler assert strings (naming feedstock)")
    w("")
    syms = tpfbin.extract_symbol_strings(img)
    w("These are what `name_functions.py` turns into a symbol map.")
    w("")
    w("| kind | count |")
    w("|---|---|")
    w("| MSVC `__FUNCSIG__` signatures | %d |" % len(syms.funcsig))
    w("| Clang/GCC `__PRETTY_FUNCTION__` signatures | %d |" % len(syms.pretty))
    w("| `__FILE__` source paths | %d |" % len(syms.files))
    w("")
    if syms.files:
        _, prefix = tpfbin.strip_source_prefix(list(syms.files.values()))
        if prefix:
            w("Common source-path prefix: `%s`" % prefix)
            w("")

    # -- TPF2 lineage -------------------------------------------------------- #
    w("## 10. TPF2-era engine symbols")
    w("")
    w("Whether TPF3 shares the TPF2 command pipeline and naming (docs/ARCHITECTURE.md "
      "open questions).")
    w("")
    w("| token | occurrences |")
    w("|---|---|")
    lowered = data
    for tok in tpfbin.TPF2_TOKENS:
        c = lowered.count(tok.encode("ascii"))
        w("| `%s` | %d |" % (tok, c))
    w("")

    # -- Mach-O signing ------------------------------------------------------ #
    if img.fmt == "macho":
        w("## 11. Code signature (macOS)")
        w("")
        flags = img.extra.get("macho_flags_list", [])
        w("- Mach-O header flags: %s" % (", ".join(flags) if flags else "(none parsed)"))
        w("- Has LC_CODE_SIGNATURE: %s" % img.extra.get("has_code_signature"))
        hardened = any("RUNTIME" in f for f in flags)  # MH_... does not carry hardened; runtime is in signature
        w("- Hardened runtime / library validation and entitlements live in the "
          "signature blob (CS flags `runtime`, `library-validation`), which LIEF "
          "does not always expose.")
        ent = img.extra.get("entitlements")
        if ent:
            w("")
            w("Entitlements found in the signature blob:")
            w("")
            w("```xml")
            w(ent.strip())
            w("```")
        else:
            w("")
            w("Entitlements were not parseable here. Run the platform tool "
              "(read-only) to read the CS flags, hardened-runtime bit and "
              "entitlements, which decide whether `DYLD_INSERT_LIBRARIES` works:")
            w("")
            w("```sh")
            w("codesign -dvvv --entitlements :- '%s'" % img.path.name)
            w("# and, for the CS flags word (0x10000 = runtime, 0x2000 = library-validation):")
            w("codesign -d --verbose=4 '%s' 2>&1 | grep -E 'flags|CodeDirectory'" % img.path.name)
            w("```")
        w("")

    if img.warnings:
        w("## Warnings")
        w("")
        for wn in img.warnings:
            w("- %s" % wn)
        w("")

    return "\n".join(L) + "\n"


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(
        description="Static, read-only survey of a Transport Fever binary (PE/ELF/Mach-O).")
    ap.add_argument("binary", type=Path, help="path to the executable or shared library")
    ap.add_argument("-o", "--output", type=Path,
                    help="write the Markdown report here (default: stdout)")
    args = ap.parse_args(argv)

    if not args.binary.is_file():
        ap.error("no such file: %s" % args.binary)
    try:
        img = tpfbin.Image.load(args.binary)
    except Exception as exc:
        print("binary_survey: cannot analyse %s: %s" % (args.binary, exc), file=sys.stderr)
        return 2
    report = build_report(img)
    if args.output:
        args.output.write_text(report, encoding="utf-8")
        print("wrote %s (%d bytes)" % (args.output, len(report)), file=sys.stderr)
    else:
        sys.stdout.write(report)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
