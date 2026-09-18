#!/usr/bin/env python3
"""selftest.py -- validate the non-PE code paths of the RE tools with fixtures.

The only real binary available before release day is Transport Fever 2 (a PE),
so the ELF (Linux) and Mach-O (macOS) paths -- including the arm64 ADRP+ADD
reference resolver the macOS build needs -- cannot be exercised against a real
file. This builds tiny synthetic fixtures in a temp directory (no game binary,
nothing launched) that each embed a `__PRETTY_FUNCTION__` and `__FILE__` string
referenced from a symtab'd function, and asserts that:

  * ELF x86-64: the rip-relative resolver names the function from its signature;
  * ELF arm64:  the ADRP+ADD resolver names the function from its signature;
  * Mach-O arm64: the structural survey parses arch/flags and, absent a readable
    code signature, emits the exact `codesign` command to run on the real build.

Run any time (no arguments):  python tools/re/selftest.py
Exit 0 when every check passes, 1 otherwise.
"""

from __future__ import annotations

import struct
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import tpfbin  # noqa: E402
import name_functions as nf  # noqa: E402
import binary_survey as bs  # noqa: E402

PRETTY = b"void GameSim::Step(long, int)\x00"
FILESTR = b"ug/train_fever/src/game/gamesim.cpp\x00"


def _align(n: int, a: int) -> int:
    return (n + a - 1) & ~(a - 1)


def build_elf(machine: int) -> bytes:
    """Minimal ELF64 (machine 62=x86_64, 183=aarch64) with a named function."""
    ehsize, phsize, shsize, symsize = 64, 56, 64, 24
    text_off = _align(ehsize + phsize, 16)
    text_size = 15 if machine == 62 else 12
    rodata_off = _align(text_off + text_size, 16)
    pretty_va, file_va = rodata_off, rodata_off + len(PRETTY)
    rodata = PRETTY + FILESTR
    symtab_off = _align(rodata_off + len(rodata), 8)
    strtab = b"\x00GameSim::Step\x00"
    symtab = struct.pack("<IBBHQQ", 0, 0, 0, 0, 0, 0)
    symtab += struct.pack("<IBBHQQ", 1, 0x12, 0, 1, text_off, text_size)  # STT_FUNC in .text
    strtab_off = symtab_off + len(symtab)
    shstr = b"\x00.text\x00.rodata\x00.symtab\x00.strtab\x00.shstrtab\x00"

    def sn(name: bytes) -> int:
        return shstr.index(b"\x00" + name + b"\x00") + 1

    shstrtab_off = strtab_off + len(strtab)
    shoff = _align(shstrtab_off + len(shstr), 8)

    if machine == 62:  # lea rax,[rip+pretty]; lea rcx,[rip+file]; ret
        text = (b"\x48\x8d\x05" + struct.pack("<i", pretty_va - (text_off + 7))
                + b"\x48\x8d\x0d" + struct.pack("<i", file_va - (text_off + 14)) + b"\xc3")
    else:  # adrp x0,#page(pretty); add x0,x0,#lo12(pretty); ret (resolver base = func RVA)
        page = text_off & ~0xFFF
        imm = ((pretty_va & ~0xFFF) - page) >> 12
        adrp = (1 << 31) | ((imm & 3) << 29) | (0b10000 << 24) | (((imm >> 2) & 0x7FFFF) << 5)
        add = 0x91000000 | ((pretty_va & 0xFFF) << 10)
        text = struct.pack("<III", adrp, add, 0xD65F03C0)
    assert len(text) == text_size

    size = _align(shoff + 6 * shsize, 16)
    buf = bytearray(size)
    e_ident = b"\x7fELF" + bytes([2, 1, 1, 0]) + b"\x00" * 8
    struct.pack_into("<16sHHIQQQIHHHHHH", buf, 0, e_ident, 3, machine, 1,
                     text_off, ehsize, shoff, 0, ehsize, phsize, 1, shsize, 6, 5)
    struct.pack_into("<IIQQQQQQ", buf, ehsize, 1, 5, 0, 0, 0, size, size, 0x1000)
    buf[text_off:text_off + len(text)] = text
    buf[rodata_off:rodata_off + len(rodata)] = rodata
    buf[symtab_off:symtab_off + len(symtab)] = symtab
    buf[strtab_off:strtab_off + len(strtab)] = strtab
    buf[shstrtab_off:shstrtab_off + len(shstr)] = shstr

    def shdr(i, nm, typ, flags, off, sz, link=0, info=0, al=1, ent=0):
        struct.pack_into("<IIQQQQIIQQ", buf, shoff + i * 64, nm, typ, flags, off, off, sz,
                         link, info, al, ent)
    shdr(1, sn(b".text"), 1, 0x6, text_off, text_size, al=16)
    shdr(2, sn(b".rodata"), 1, 0x2, rodata_off, len(rodata), al=16)
    shdr(3, sn(b".symtab"), 2, 0, symtab_off, len(symtab), link=4, info=1, al=8, ent=symsize)
    shdr(4, sn(b".strtab"), 3, 0, strtab_off, len(strtab))
    shdr(5, sn(b".shstrtab"), 3, 0, shstrtab_off, len(shstr))
    # fix .symtab/.strtab sh_addr to 0 (not allocated); shdr() set addr=off, correct that
    struct.pack_into("<Q", buf, shoff + 3 * 64 + 16, 0)
    struct.pack_into("<Q", buf, shoff + 4 * 64 + 16, 0)
    struct.pack_into("<Q", buf, shoff + 5 * 64 + 16, 0)
    return bytes(buf)


def build_macho_arm64() -> bytes:
    """Minimal Mach-O arm64 with __text and a __cstring holding the signature."""
    buf = bytearray(0x1000)
    text_off, cstr_off = 0x200, 0x240
    text = struct.pack("<I", 0xD65F03C0)  # ret

    def sect(name, seg, addr, size, off, flags):
        return struct.pack("<16s16sQQIIIIIII4x", name.encode(), seg.encode(), addr, size,
                           off, 0, 0, 0, flags, 0, 0)
    sects = sect("__text", "__TEXT", text_off, len(text), text_off, 0x80000400)
    sects += sect("__cstring", "__TEXT", cstr_off, len(PRETTY), cstr_off, 2)
    segbody = struct.pack("<16sQQQQIIII", b"__TEXT", 0, 0x1000, 0, 0x1000, 5, 5, 2, 0) + sects
    lc = struct.pack("<II", 0x19, 8 + len(segbody)) + segbody  # LC_SEGMENT_64
    struct.pack_into("<IIIIIIII", buf, 0, 0xFEEDFACF, 0x0100000C, 0, 2, 1, len(lc), 0x00200085, 0)
    buf[32:32 + len(lc)] = lc
    buf[text_off:text_off + len(text)] = text
    buf[cstr_off:cstr_off + len(PRETTY)] = PRETTY
    return bytes(buf)


def _check(cond: bool, msg: str, failures: list[str]) -> None:
    print(("  PASS " if cond else "  FAIL ") + msg)
    if not cond:
        failures.append(msg)


def main() -> int:
    failures: list[str] = []
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)

        # -- ELF x86_64 -- rip-relative resolver
        p = tmp / "fx_x86_64.elf"
        p.write_bytes(build_elf(62))
        img = tpfbin.Image.load(p)
        syms = tpfbin.extract_symbol_strings(img)
        funcs, stats = nf.build_symbol_map(img, syms)
        named = [f for f in funcs.values() if f.name]
        print("ELF x86_64:")
        _check(img.arch == "x86_64", "arch is x86_64", failures)
        _check(stats["assert_pretty_strings"] == 1, "found the __PRETTY_FUNCTION__ string", failures)
        _check(any(f.name == "GameSim::Step" for f in named),
               "named GameSim::Step via rip-relative reference", failures)
        _check(any("gamesim.cpp" in f.source_file for f in funcs.values()),
               "attributed to gamesim.cpp", failures)

        # -- ELF arm64 -- ADRP+ADD resolver
        p = tmp / "fx_arm64.elf"
        p.write_bytes(build_elf(183))
        img = tpfbin.Image.load(p)
        syms = tpfbin.extract_symbol_strings(img)
        funcs, stats = nf.build_symbol_map(img, syms)
        print("ELF arm64:")
        _check(img.arch == "aarch64", "arch is aarch64", failures)
        _check(any(f.name == "GameSim::Step" for f in funcs.values() if f.name),
               "named GameSim::Step via ADRP+ADD reference", failures)

        # -- Mach-O arm64 -- structural survey + codesign fallback
        p = tmp / "fx_arm64.macho"
        p.write_bytes(build_macho_arm64())
        img = tpfbin.Image.load(p)
        report = bs.build_report(img)
        print("Mach-O arm64:")
        _check(img.fmt == "macho" and img.arch == "aarch64", "parsed as Mach-O aarch64", failures)
        _check("Code signature (macOS)" in report, "survey has the macOS code-signature section", failures)
        _check("codesign -dvvv" in report,
               "survey emits the codesign command when entitlements are unreadable", failures)

    print()
    if failures:
        print("SELFTEST FAILED (%d check(s)):" % len(failures))
        for m in failures:
            print("  - " + m)
        return 1
    print("SELFTEST PASSED: ELF x86_64 / ELF arm64 / Mach-O arm64 paths OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
