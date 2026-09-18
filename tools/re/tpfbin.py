"""Shared, read-only binary model for the TPF3-MP recon tools.

`binary_survey.py` and `name_functions.py` both rest on this module. It never
launches, writes to, or modifies a target; it only reads bytes.

Why one module, three formats: Transport Fever 3 ships native builds for
Windows x64 (PE), Linux x64 (ELF) and macOS arm64 (Mach-O) on the same day, so
every tool has to open all three from the first hour. The structural parsing is
format-specific (PE via `pefile`, ELF/Mach-O via `lief`), but the two techniques
that actually name functions -- scanning read-only data for compiler assert
strings, and finding the code that references them -- are shared here and are
format-independent given a section list.

The TPF2 baseline (build 35924, image base 0x140000000) is the only binary this
can be validated against before release day; see `investigation/tpf2-baseline/`.
"""

from __future__ import annotations

import dataclasses
import math
import re
import struct
from pathlib import Path
from typing import Iterable, Optional

# `pefile` is required (PE is the format available for validation today).
# `lief` is required for ELF and Mach-O, which only exist on release day.
try:
    import pefile
except ImportError as exc:  # pragma: no cover - environment guard
    raise SystemExit(
        "tpfbin requires the 'pefile' package (pip install -r tools/requirements.txt): %s" % exc
    )

try:
    import lief
    # Keep LIEF's parser chatter off stdout/stderr so tool output stays clean and
    # deterministic; real problems still surface as exceptions or img.warnings.
    try:
        lief.logging.disable()
    except Exception:
        pass
except ImportError:  # ELF/Mach-O paths degrade to an explicit failure, PE still works.
    lief = None  # type: ignore


# --------------------------------------------------------------------------- #
# Format detection                                                            #
# --------------------------------------------------------------------------- #

def detect_format(data: bytes) -> str:
    """Return 'pe', 'elf', 'macho', 'macho-fat' or 'unknown' from magic bytes."""
    if len(data) < 4:
        return "unknown"
    if data[:2] == b"MZ":
        # Confirm the PE signature the DOS stub points at, so a bare DOS exe
        # (or a non-PE .MZ) is not mistaken for a modern image.
        if len(data) >= 0x40:
            pe_off = struct.unpack_from("<I", data, 0x3C)[0]
            if pe_off + 4 <= len(data) and data[pe_off:pe_off + 4] == b"PE\x00\x00":
                return "pe"
        return "unknown"
    if data[:4] == b"\x7fELF":
        return "elf"
    if data[:4] in (b"\xca\xfe\xba\xbe", b"\xbe\xba\xfe\xca"):
        return "macho-fat"  # universal (fat) binary: multiple slices
    if data[:4] in (b"\xfe\xed\xfa\xce", b"\xce\xfa\xed\xfe",
                    b"\xfe\xed\xfa\xcf", b"\xcf\xfa\xed\xfe"):
        return "macho"
    return "unknown"


# --------------------------------------------------------------------------- #
# Data model                                                                  #
# --------------------------------------------------------------------------- #

@dataclasses.dataclass
class Section:
    name: str
    vaddr: int          # RVA (relative to image base)
    vsize: int
    file_off: int
    file_size: int
    flags: int
    entropy: float
    readable: bool
    writable: bool
    executable: bool


@dataclasses.dataclass
class ImportedLib:
    name: str
    symbols: list[str]
    from_image_dir: bool = False  # True when the DLL/so sits next to the target


@dataclasses.dataclass
class SymbolStrings:
    """Compiler assert strings recovered from read-only data.

    funcsig  : RVA -> MSVC __FUNCSIG__ (a full demangled C++ signature that
               carries a calling convention, e.g. `struct Command __cdecl
               make_cmd::BuildProposal(...)`).
    pretty   : RVA -> Clang/GCC __PRETTY_FUNCTION__ (a signature with no calling
               convention, what the Linux and macOS builds embed instead).
    files    : RVA -> __FILE__ source path.
    """
    funcsig: dict[int, str]
    pretty: dict[int, str]
    files: dict[int, str]


@dataclasses.dataclass
class Ref:
    insn_rva: int       # address of the referencing instruction
    target_rva: int     # RVA of the string it points at


# --------------------------------------------------------------------------- #
# String classification                                                       #
# --------------------------------------------------------------------------- #

# A calling-convention token marks an MSVC __FUNCSIG__ unambiguously.
_MSVC_CC = re.compile(rb"__(?:cdecl|thiscall|stdcall|fastcall|vectorcall|clrcall)\b")
# Source-file suffixes for a __FILE__ literal.
_SRC_SUFFIX = re.compile(rb"\.(?:cpp|cc|cxx|c|h|hh|hpp|hxx|inl|ipp)$", re.IGNORECASE)
# A Clang/GCC __PRETTY_FUNCTION__ looks like `<ret> <qualified name>(<args>)`
# possibly with trailing cv/ref qualifiers. There is no calling convention, so
# it is only accepted when it also names a scope (`::`) or an obvious C++ shape,
# to keep ordinary sentences out of the set.
_PRETTY = re.compile(
    rb"^[A-Za-z_][\w :<>,\*&~\[\]]*\s[\w:<>~]+\([^;{}\n]*\)(?:\s*const)?(?:\s*noexcept)?$"
)


def classify_string(raw: bytes) -> Optional[str]:
    """Return 'funcsig', 'pretty', 'file' or None for one candidate string."""
    if b"(" in raw and _MSVC_CC.search(raw):
        return "funcsig"
    if _SRC_SUFFIX.search(raw) and (b"\\" in raw or b"/" in raw):
        return "file"
    if b"(" in raw and b"::" in raw and 6 <= len(raw) <= 1024 and _PRETTY.match(raw):
        return "pretty"
    return None


def _iter_c_strings(blob: bytes, base_rva: int) -> Iterable[tuple[int, bytes]]:
    """Yield (rva, bytes) for every NUL-terminated printable run at a NUL boundary."""
    for m in re.finditer(rb"[\x20-\x7e]{4,}\x00", blob):
        start = m.start()
        if start > 0 and blob[start - 1] != 0:
            continue  # only strings that begin on a NUL boundary (real literals)
        yield base_rva + start, m.group()[:-1]


# --------------------------------------------------------------------------- #
# Entropy                                                                      #
# --------------------------------------------------------------------------- #

def shannon_entropy(blob: bytes) -> float:
    """Shannon entropy in bits/byte (0..8). 0 for empty input."""
    if not blob:
        return 0.0
    counts = [0] * 256
    for b in blob:
        counts[b] += 1
    n = len(blob)
    ent = 0.0
    for c in counts:
        if c:
            p = c / n
            ent -= p * math.log2(p)
    return ent


# --------------------------------------------------------------------------- #
# Image                                                                        #
# --------------------------------------------------------------------------- #

_PE_MACHINE = {
    0x8664: "x86_64",
    0x14C: "x86",
    0xAA64: "aarch64",
    0x1C0: "arm",
    0x1C4: "armnt",
}


class Image:
    """A parsed, read-only executable image with a format-agnostic surface."""

    def __init__(self, path: Path, data: bytes, fmt: str) -> None:
        self.path = path
        self.data = data
        self.fmt = fmt
        self.arch = "unknown"
        self.bits = 0
        self.endianness = "little"
        self.machine_name = ""
        self.image_base = 0
        self.entrypoint_rva = 0
        self.sections: list[Section] = []
        self.imports: list[ImportedLib] = []
        self.exports: list[tuple[str, int]] = []  # (name, rva)
        self.tls_callbacks: list[int] = []         # PE: TLS callbacks; ELF/Mach-O: initializers
        self.tls_label = "TLS callbacks"
        self._func_begins: list[int] = []
        self._func_end: dict[int, int] = {}
        self.pe: Optional["pefile.PE"] = None
        self.lief = None
        self.extra: dict = {}                      # format-specific detail
        self.warnings: list[str] = []

    # -- construction -------------------------------------------------------- #

    @classmethod
    def load(cls, path: str | Path, image_dir_names: Optional[set[str]] = None) -> "Image":
        p = Path(path)
        data = p.read_bytes()
        fmt = detect_format(data)
        if fmt == "unknown":
            raise ValueError("%s: unrecognised binary format (not PE/ELF/Mach-O)" % p)
        if fmt == "macho-fat":
            raise ValueError(
                "%s: Mach-O universal (fat) binary; extract the arm64 slice first "
                "(e.g. `lipo -thin arm64`) and pass that" % p
            )
        img = cls(p, data, fmt)
        siblings = image_dir_names
        if siblings is None:
            siblings = {f.name.lower() for f in p.parent.iterdir()} if p.parent.is_dir() else set()
        if fmt == "pe":
            img._load_pe(siblings)
        else:
            img._load_lief(siblings)
        return img

    # -- PE ------------------------------------------------------------------ #

    def _load_pe(self, siblings: set[str]) -> None:
        pe = pefile.PE(data=self.data, fast_load=True)
        pe.parse_data_directories(directories=[
            pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_IMPORT"],
            pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_EXPORT"],
            pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_TLS"],
            pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT"],
        ])
        self.pe = pe
        self.image_base = pe.OPTIONAL_HEADER.ImageBase
        self.entrypoint_rva = pe.OPTIONAL_HEADER.AddressOfEntryPoint
        self.machine_name = _PE_MACHINE.get(pe.FILE_HEADER.Machine, "0x%x" % pe.FILE_HEADER.Machine)
        self.arch = self.machine_name
        self.bits = 64 if pe.OPTIONAL_HEADER.Magic == 0x20B else 32
        for s in pe.sections:
            name = s.Name.rstrip(b"\0").decode("ascii", "replace")
            ch = s.Characteristics
            blob = self.data[s.PointerToRawData:s.PointerToRawData + s.SizeOfRawData]
            self.sections.append(Section(
                name=name, vaddr=s.VirtualAddress, vsize=s.Misc_VirtualSize,
                file_off=s.PointerToRawData, file_size=s.SizeOfRawData, flags=ch,
                entropy=shannon_entropy(blob),
                readable=bool(ch & 0x40000000), writable=bool(ch & 0x80000000),
                executable=bool(ch & 0x20000000)))
        # imports
        for imp in getattr(pe, "DIRECTORY_ENTRY_IMPORT", []):
            dll = imp.dll.decode("ascii", "replace") if imp.dll else "?"
            names = [(i.name.decode("ascii", "replace") if i.name else "#%d" % (i.ordinal or 0))
                     for i in imp.imports]
            self.imports.append(ImportedLib(dll, names, dll.lower() in siblings))
        for imp in getattr(pe, "DIRECTORY_ENTRY_DELAY_IMPORT", []):
            dll = imp.dll.decode("ascii", "replace") if imp.dll else "?"
            names = [(i.name.decode("ascii", "replace") if i.name else "#%d" % (i.ordinal or 0))
                     for i in imp.imports]
            self.imports.append(ImportedLib(dll + " (delay)", names, dll.lower() in siblings))
        # exports
        exp = getattr(pe, "DIRECTORY_ENTRY_EXPORT", None)
        if exp:
            for s in exp.symbols:
                if s.name:
                    self.exports.append((s.name.decode("ascii", "replace"), s.address))
        # TLS callbacks
        tls = getattr(pe, "DIRECTORY_ENTRY_TLS", None)
        if tls and tls.struct.AddressOfCallBacks:
            rva = tls.struct.AddressOfCallBacks - self.image_base
            for k in range(64):
                v = self._read_qword(rva + 8 * k)
                if not v:
                    break
                self.tls_callbacks.append(v - self.image_base)
        self._pe_function_bounds()
        self._pe_extra()

    def _pe_function_bounds(self) -> None:
        """Primary function begins/ends from .pdata, resolving chained unwind info.

        On x64 PE the linker publishes one RUNTIME_FUNCTION per code range in
        .pdata (the exception directory). A range whose UNWIND_INFO carries
        UNW_FLAG_CHAININFO (or the self-referential indirection form) is a
        continuation of another function; following the chain collapses those
        onto the primary entry, which is the function boundary a hook cares
        about. This is exactly the boundary source `find_sym.py`/`tpfdis.py` use
        in the prior art.
        """
        pe = self.pe
        d = pe.OPTIONAL_HEADER.DATA_DIRECTORY[pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_EXCEPTION"]]
        if not d.Size:
            self.warnings.append("no .pdata/exception directory; function bounds unavailable")
            return
        off = pe.get_offset_from_rva(d.VirtualAddress)
        n = d.Size // 12
        entries = struct.unpack_from("<%dI" % (n * 3), self.data, off)
        parent_end: dict[int, int] = {}
        for i in range(n):
            begin, end, unwind = entries[3 * i], entries[3 * i + 1], entries[3 * i + 2]
            cur_b, cur_e, cur_u = begin, end, unwind
            for _ in range(64):
                if cur_u & 1:  # self-referential indirection to another RUNTIME_FUNCTION
                    uoff = self._rva_to_off(cur_u - 1)
                    if uoff is None:
                        break
                    cur_b, cur_e, cur_u = struct.unpack_from("<III", self.data, uoff)
                    continue
                uoff = self._rva_to_off(cur_u)
                if uoff is None:
                    break
                verflags = self.data[uoff]
                flags = verflags >> 3
                count = self.data[uoff + 2]
                if flags & 0x4:  # UNW_FLAG_CHAININFO -> chained RUNTIME_FUNCTION follows codes
                    p = uoff + 4 + ((count + 1) & ~1) * 2
                    cur_b, cur_e, cur_u = struct.unpack_from("<III", self.data, p)
                    continue
                break
            prev = parent_end.get(cur_b)
            parent_end[cur_b] = max(prev, end) if prev else end
        self._func_end = parent_end
        self._func_begins = sorted(parent_end)

    def _pe_extra(self) -> None:
        pe = self.pe
        self.extra["dll_characteristics"] = pe.OPTIONAL_HEADER.DllCharacteristics
        self.extra["timestamp"] = pe.FILE_HEADER.TimeDateStamp
        self.extra["linker"] = "%d.%d" % (pe.OPTIONAL_HEADER.MajorLinkerVersion,
                                          pe.OPTIONAL_HEADER.MinorLinkerVersion)
        self.extra["image_size"] = pe.OPTIONAL_HEADER.SizeOfImage
        # overlay (bytes past the last section)
        ov = pe.get_overlay_data_start_offset()
        self.extra["overlay_off"] = ov
        self.extra["overlay_size"] = (len(self.data) - ov) if ov is not None else 0
        # Rich header (MSVC toolchain fingerprint)
        try:
            rich = pe.parse_rich_header()
        except Exception:
            rich = None
        if rich:
            comps = []
            vals = rich["values"]
            for i in range(0, len(vals), 2):
                comp = vals[i]
                comps.append((comp >> 16, comp & 0xFFFF, vals[i + 1]))
            self.extra["rich"] = comps
        # CodeView PDB path
        try:
            pe.parse_data_directories(
                directories=[pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_DEBUG"]])
            for dbg in getattr(pe, "DIRECTORY_ENTRY_DEBUG", []):
                e = dbg.entry
                if e and hasattr(e, "PdbFileName"):
                    self.extra["pdb"] = e.PdbFileName.rstrip(b"\0").decode("ascii", "replace")
                    break
        except Exception:
            pass
        # security directory (Authenticode)
        sec = pe.OPTIONAL_HEADER.DATA_DIRECTORY[
            pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_SECURITY"]]
        self.extra["authenticode"] = sec.Size > 0

    # -- ELF / Mach-O via LIEF ---------------------------------------------- #

    def _load_lief(self, siblings: set[str]) -> None:
        if lief is None:
            raise SystemExit(
                "parsing %s needs the 'lief' package (pip install -r tools/requirements.txt)"
                % self.fmt.upper())
        b = lief.parse(str(self.path))
        if b is None:
            raise ValueError("%s: lief could not parse the %s image" % (self.path, self.fmt))
        self.lief = b
        ab = b.abstract
        self.bits = 64 if ab.header.is_64 else 32
        self.endianness = "little" if "LITTLE" in str(ab.header.endianness) else "big"
        arch = str(ab.header.architecture).rsplit(".", 1)[-1].lower()
        self.arch = {"x86_64": "x86_64", "arm64": "aarch64", "aarch64": "aarch64",
                     "arm": "arm", "x86": "x86", "i386": "x86"}.get(arch, arch)
        self.machine_name = self.arch
        self.image_base = b.imagebase
        self.entrypoint_rva = ab.entrypoint - b.imagebase
        for s in b.sections:
            try:
                content = bytes(s.content)
            except Exception:
                content = b""
            va = s.virtual_address
            # LIEF reports some ELF section addresses as absolute; normalise to RVA.
            rva = va - self.image_base if va >= self.image_base else va
            self.sections.append(Section(
                name=s.name, vaddr=rva, vsize=int(getattr(s, "virtual_size", 0) or getattr(s, "size", 0)),
                file_off=int(getattr(s, "offset", 0) or getattr(s, "pointerto_raw_data", 0)),
                file_size=len(content), flags=0,
                entropy=shannon_entropy(content) if content else 0.0,
                readable=True, writable=self._lief_writable(s), executable=self._lief_exec(s)))
        self._lief_imports(b, siblings)
        self._lief_exports(b)
        self._lief_functions(b)
        if self.fmt == "elf":
            self._elf_extra(b)
        else:
            self._macho_extra(b)

    @staticmethod
    def _lief_exec(section) -> bool:
        flags = str(getattr(section, "flags", "")) + str(list(getattr(section, "characteristics_lists", []) or []))
        if "EXECINSTR" in flags or "MEM_EXECUTE" in flags:
            return True
        for f in getattr(section, "flags_list", []) or []:
            if "EXEC" in str(f):
                return True
        return section.name in (".text", "__text")

    @staticmethod
    def _lief_writable(section) -> bool:
        flags = str(getattr(section, "flags", ""))
        for f in getattr(section, "flags_list", []) or []:
            flags += str(f)
        return "WRITE" in flags.upper()

    def _lief_imports(self, b, siblings: set[str]) -> None:
        if self.fmt == "elf":
            libs = [str(x) for x in getattr(b, "libraries", [])]
            undef = []
            for sym in b.symbols:
                if getattr(sym, "is_imported", False) or (
                        not getattr(sym, "is_exported", True) and sym.name and int(getattr(sym, "value", 0)) == 0):
                    if sym.name:
                        undef.append(sym.name)
            for lib in libs:
                self.imports.append(ImportedLib(lib, [], lib.lower() in siblings))
            if undef:
                self.imports.append(ImportedLib("(undefined symbols)", sorted(set(undef))))
        else:  # Mach-O
            for lib in getattr(b, "libraries", []):
                name = lib.name if hasattr(lib, "name") else str(lib)
                base = name.rsplit("/", 1)[-1]
                self.imports.append(ImportedLib(name, [], base.lower() in siblings))
            undef = [s.name for s in b.symbols
                     if getattr(s, "is_external", False) and not int(getattr(s, "value", 0))]
            if undef:
                self.imports.append(ImportedLib("(undefined symbols)", sorted(set(n for n in undef if n))))

    def _lief_exports(self, b) -> None:
        try:
            for f in b.abstract.exported_functions:
                name = f.name if hasattr(f, "name") else str(f)
                addr = int(getattr(f, "address", 0))
                if name:
                    self.exports.append((name, addr - self.image_base if addr >= self.image_base else addr))
        except Exception as exc:
            self.warnings.append("export enumeration failed: %s" % exc)

    def _lief_functions(self, b) -> None:
        begins: set[int] = set()
        addr_list: list[int] = []
        for f in getattr(b, "functions", []) or []:
            a = int(getattr(f, "address", 0))
            if a:
                rva = a - self.image_base if a >= self.image_base else a
                begins.add(rva)
                addr_list.append(rva)
        # size hints, where LIEF has them
        end: dict[int, int] = {}
        for f in getattr(b, "functions", []) or []:
            a = int(getattr(f, "address", 0))
            size = int(getattr(f, "size", 0) or 0)
            if a and size:
                rva = a - self.image_base if a >= self.image_base else a
                end[rva] = rva + size
        self._func_begins = sorted(begins)
        # fill unknown ends with the next begin (bounded, conservative)
        for i, bgn in enumerate(self._func_begins):
            if bgn in end:
                continue
            nxt = self._func_begins[i + 1] if i + 1 < len(self._func_begins) else None
            end[bgn] = nxt if nxt else bgn + 0x40
        self._func_end = end
        if not self._func_begins:
            self.warnings.append(
                "no function starts (LC_FUNCTION_STARTS / symbols / eh_frame) found; "
                "function attribution will fall back to string proximity only")

    def _elf_extra(self, b) -> None:
        self.tls_label = "init/init_array"
        inits: list[int] = []
        for name in ("init_array", "preinit_array"):
            try:
                arr = getattr(b, name)
                for a in arr:
                    inits.append(int(a) - self.image_base if int(a) >= self.image_base else int(a))
            except Exception:
                pass
        self.tls_callbacks = inits
        self.extra["is_pie"] = bool(getattr(b, "is_pie", False))
        self.extra["nx"] = bool(getattr(b.abstract, "has_nx", False))
        try:
            self.extra["interpreter"] = b.interpreter
        except Exception:
            pass
        try:
            bid = b.get_section(".note.gnu.build-id")
            if bid:
                self.extra["build_id"] = bytes(bid.content).hex()
        except Exception:
            pass

    def _macho_extra(self, b) -> None:
        self.tls_label = "initializers (mod_init_func / LC_MAIN)"
        hdr = b.header
        self.extra["macho_flags"] = int(hdr.flags)
        self.extra["macho_flags_list"] = [str(f).rsplit(".", 1)[-1]
                                          for f in getattr(hdr, "flags_list", [])]
        self.extra["has_code_signature"] = bool(getattr(b, "has_code_signature", False))
        cs = getattr(b, "code_signature", None)
        if cs is not None:
            self.extra["code_signature_size"] = int(getattr(cs, "data_size", 0) or 0)
        # entitlements / hardened runtime are carried in the signature blob; LIEF
        # does not always parse them, so the survey documents the exact codesign
        # command when they cannot be read here.
        ct = getattr(b, "code_signature", None)
        self.extra["entitlements"] = None
        try:
            if hasattr(b, "code_signature") and hasattr(ct, "content"):
                blob = bytes(ct.content)
                m = re.search(rb"<\?xml.*?</plist>", blob, re.DOTALL)
                if m:
                    self.extra["entitlements"] = m.group().decode("utf-8", "replace")
        except Exception:
            pass

    # -- address helpers ----------------------------------------------------- #

    def _rva_to_off(self, rva: int) -> Optional[int]:
        for s in self.sections:
            if s.vaddr <= rva < s.vaddr + max(s.vsize, s.file_size):
                delta = rva - s.vaddr
                if delta < s.file_size:
                    return s.file_off + delta
                return None
        return None

    def _read_qword(self, rva: int) -> Optional[int]:
        off = self._rva_to_off(rva)
        if off is None or off + 8 > len(self.data):
            return None
        return struct.unpack_from("<Q", self.data, off)[0]

    def read_rva(self, rva: int, n: int) -> Optional[bytes]:
        off = self._rva_to_off(rva)
        if off is None:
            return None
        return self.data[off:off + n]

    def read_cstring(self, rva: int, maxlen: int = 512) -> Optional[str]:
        off = self._rva_to_off(rva)
        if off is None:
            return None
        end = self.data.find(b"\0", off, off + maxlen)
        if end < 0:
            end = off + maxlen
        try:
            return self.data[off:end].decode("ascii")
        except UnicodeDecodeError:
            return None

    # -- sections of interest ----------------------------------------------- #

    def code_sections(self) -> list[tuple[int, bytes]]:
        out = []
        for s in self.sections:
            if s.executable and s.file_size:
                out.append((s.vaddr, self.data[s.file_off:s.file_off + s.file_size]))
        return out

    def readonly_data_sections(self) -> list[tuple[int, bytes]]:
        """Non-executable readable sections that can hold assert strings."""
        out = []
        for s in self.sections:
            if s.executable or not s.file_size:
                continue
            if not s.readable:
                continue
            out.append((s.vaddr, self.data[s.file_off:s.file_off + s.file_size]))
        return out

    # -- function containment ------------------------------------------------ #

    @property
    def function_starts(self) -> list[int]:
        return self._func_begins

    def function_end(self, begin: int) -> Optional[int]:
        return self._func_end.get(begin)

    def containing_function(self, rva: int) -> Optional[int]:
        import bisect
        i = bisect.bisect_right(self._func_begins, rva) - 1
        if i < 0:
            return None
        b = self._func_begins[i]
        end = self._func_end.get(b, 0)
        return b if b <= rva < end else None


# --------------------------------------------------------------------------- #
# Assert-string extraction (format-independent, given a section list)         #
# --------------------------------------------------------------------------- #

def extract_symbol_strings(image: Image) -> SymbolStrings:
    funcsig: dict[int, str] = {}
    pretty: dict[int, str] = {}
    files: dict[int, str] = {}
    for base, blob in image.readonly_data_sections():
        for rva, raw in _iter_c_strings(blob, base):
            kind = classify_string(raw)
            if kind is None:
                continue
            text = raw.decode("ascii", "replace")
            if kind == "funcsig":
                funcsig[rva] = text
            elif kind == "pretty":
                pretty[rva] = text
            else:
                files[rva] = text
    return SymbolStrings(funcsig, pretty, files)


# --------------------------------------------------------------------------- #
# Reference resolution                                                        #
# --------------------------------------------------------------------------- #

def find_rip_relative_refs(image: Image, targets: set[int]) -> list[Ref]:
    """Every `lea/mov reg, [rip+disp32]` in code that points at a target RVA.

    x86-64 (PE Windows, ELF Linux). Uses a vectorised scan for candidate ModR/M
    bytes, then keeps only displacements that land on a target. The one-pass
    approach is what makes this run in seconds on a ~50 MB .text; the caller
    verifies instruction boundaries where exactness matters.
    """
    import numpy as np
    if not targets:
        return []
    tgt_arr = np.array(sorted(targets), dtype=np.int64)
    refs: list[Ref] = []
    for base, blob in image.code_sections():
        code = np.frombuffer(blob, dtype=np.uint8)
        if len(code) < 6:
            continue
        # ModR/M with mod=00, r/m=101 => RIP-relative disp32 follows.
        mp = np.nonzero((code[:-4] & 0xC7) == 0x05)[0]
        if not len(mp):
            continue
        disp = (code[mp + 1].astype(np.int64)
                | (code[mp + 2].astype(np.int64) << 8)
                | (code[mp + 3].astype(np.int64) << 16)
                | (code[mp + 4].astype(np.int64) << 24))
        disp = np.where(disp >= 2 ** 31, disp - 2 ** 32, disp)
        end_rva = base + mp + 5  # rip points just past the disp32
        tgt = end_rva + disp
        hit = np.isin(tgt, tgt_arr)
        for p, t in zip(mp[hit], tgt[hit]):
            refs.append(Ref(insn_rva=base + int(p) + 1, target_rva=int(t)))
    return refs


def find_arm64_adr_refs(image: Image, targets: set[int]) -> list[Ref]:
    """Every ADRP(+ADD/LDR) page-relative reference that resolves to a target.

    arm64 (macOS). An address literal is materialised as ADRP xN, #page followed
    by ADD xN, xN, #lo12 (or LDR with a scaled offset). Decode per function -- the
    function starts from LC_FUNCTION_STARTS bound the work, so capstone never has
    to disassemble the whole __text at once.
    """
    if not targets:
        return []
    try:
        from capstone import Cs, CS_ARCH_ARM64, CS_MODE_LITTLE_ENDIAN
        from capstone.arm64 import ARM64_OP_IMM, ARM64_OP_REG
    except ImportError as exc:  # pragma: no cover
        image.warnings.append("capstone arm64 unavailable: %s" % exc)
        return []
    md = Cs(CS_ARCH_ARM64, CS_MODE_LITTLE_ENDIAN)
    md.detail = True
    refs: list[Ref] = []
    begins = image.function_starts
    if not begins:
        # No function starts: scan whole code sections (bounded, still per 4 bytes).
        spans = image.code_sections()
    else:
        spans = []
        for b in begins:
            e = image.function_end(b) or (b + 0x400)
            blob = image.read_rva(b, e - b)
            if blob:
                spans.append((b, blob))
    for base, blob in spans:
        pagereg: dict[int, int] = {}
        for insn in md.disasm(blob, base):
            mn = insn.mnemonic
            if mn == "adrp" and len(insn.operands) == 2:
                reg = insn.operands[0].reg
                page = insn.operands[1].imm
                pagereg[reg] = page
            elif mn == "add" and len(insn.operands) == 3:
                dst, src, imm = insn.operands
                if src.type == ARM64_OP_REG and src.reg in pagereg and imm.type == ARM64_OP_IMM:
                    tgt = pagereg[src.reg] + imm.imm
                    rva = tgt - image.image_base if tgt >= image.image_base else tgt
                    if rva in targets:
                        refs.append(Ref(insn_rva=insn.address, target_rva=rva))
            elif mn in ("ldr", "ldrsw") and len(insn.operands) == 2:
                mem = insn.operands[1]
                if mem.type != ARM64_OP_IMM and getattr(mem, "mem", None) and mem.mem.base in pagereg:
                    tgt = pagereg[mem.mem.base] + mem.mem.disp
                    rva = tgt - image.image_base if tgt >= image.image_base else tgt
                    if rva in targets:
                        refs.append(Ref(insn_rva=insn.address, target_rva=rva))
    return refs


def resolve_references(image: Image, targets: set[int]) -> list[Ref]:
    """Dispatch reference resolution on the image architecture."""
    if image.arch in ("x86_64", "x86"):
        return find_rip_relative_refs(image, targets)
    if image.arch == "aarch64":
        return find_arm64_adr_refs(image, targets)
    image.warnings.append(
        "reference resolution for arch %r is not implemented; "
        "assert strings were located but not attributed to code" % image.arch)
    return []


# --------------------------------------------------------------------------- #
# Source-path handling                                                        #
# --------------------------------------------------------------------------- #

import collections as _collections  # noqa: E402

# Build-tree roots after which a path becomes a stable, machine-independent name.
_SRC_ROOTS = ("\\src\\", "/src/", "\\urban_games\\", "/urban_games/",
              "\\include\\", "/include/", "\\ext\\", "/ext/")


def strip_source_prefix(paths: Iterable[str]) -> tuple[dict[str, str], str]:
    """Map each source path to a stable short name, and return the dominant prefix.

    Each path is cut after the last recognised build-tree root (`\\src\\`,
    `\\urban_games\\`, ...), which is what makes the name comparable across
    machines and builds (the absolute GitLab-runner prefix differs per build).
    Paths with no such root keep their leading `.\\`/drive stripped. Mixed roots
    are handled per-path rather than requiring one shared prefix.
    """
    plist = list(paths)
    if not plist:
        return {}, ""
    out: dict[str, str] = {}
    prefixes: "_collections.Counter[str]" = _collections.Counter()
    for orig in plist:
        n = orig.replace("/", "\\")
        low = n.lower()
        cut = -1
        for root in ("\\src\\", "\\urban_games\\", "\\include\\", "\\ext\\"):
            i = low.rfind(root)
            if i >= 0:
                cut = i + len(root)
                break
        if cut < 0:
            m = 0
            while m < len(low) and low[m] in ".\\":
                m += 1
            # also drop a leading drive letter like "c:\"
            if len(low) >= 2 and low[1] == ":":
                m = 3 if len(low) >= 3 else 2
            cut = m
        out[orig] = low[cut:]
        prefixes[low[:cut]] += 1
    rep = prefixes.most_common(1)[0][0] if prefixes else ""
    return out, rep


# Tokens that identify the TPF2 engine lineage (used by binary_survey).
TPF2_TOKENS = [
    "make_cmd::", "CommandList::Add", "CommandList::", "GameSim::Step",
    "CGame::RunGameSimLoop", "CGame::", "CGameTime::", "api.cmd",
    "sendCommand", "buildProposal", "TransportVehicleConfig", "ecs::Engine",
]
