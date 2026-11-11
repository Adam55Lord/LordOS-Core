"""
lordfmt.py - the Lord container family, format version 2.

  LXF = executable   LLF = library   (base header only, 64 bytes)
  LKF = kernel       LDF = driver    (base header + a type-specific extension)

All four share one container (see SPEC.md): they differ in magic, filetype and
header, plus a few validation rules; sections, relocations, imports and exports
are identical.
Design notes (inspired by PE/COFF and ELF):
  * the file is a header + section table + sections; every file offset is
    bounds-checked on load
  * *mapped* sections are loaded at an RVA (relative virtual address) with a
    separate in-memory size (virtual_size), so .bss costs no file space
  * *metadata* sections (.strtab .edata .impdir .reloc .symtab) are never
    mapped; they are generated from the high-level model on write
  * import slots (the IAT) live in a generated, separately mapped .idata
    section, so code is never writable and data is never executable (W^X)
"""
import struct
import zlib
from bisect import bisect_left
from dataclasses import dataclass

# ----------------------------------------------------------------- constants
MAGIC_LXF = b"LXF\0"
MAGIC_LLF = b"LLF\0"
MAGIC_LKF = b"LKF\0"
MAGIC_LDF = b"LDF\0"
FORMAT_VERSION = 2
LOADER_VERSION = 3            # highest min_loader value this code understands
KERNEL_MIN_LOADER = 3         # LKF/LDF files require at least this loader

TYPE_EXECUTABLE = 1
TYPE_LIBRARY = 2
TYPE_KERNEL = 3
TYPE_DRIVER = 4
TYPE_BY_MAGIC = {MAGIC_LXF: TYPE_EXECUTABLE, MAGIC_LLF: TYPE_LIBRARY,
                 MAGIC_LKF: TYPE_KERNEL, MAGIC_LDF: TYPE_DRIVER}
MAGIC_BY_TYPE = {v: k for k, v in TYPE_BY_MAGIC.items()}
TYPE_LABEL = {TYPE_EXECUTABLE: "executable (LXF)", TYPE_LIBRARY: "library (LLF)",
              TYPE_KERNEL: "kernel (LKF)", TYPE_DRIVER: "driver (LDF)"}
ENTRY_TYPES = (TYPE_EXECUTABLE, TYPE_KERNEL)     # file types that have an entry point
ARCH_X86_64 = 1

FLAG_HAS_ENTRY = 0x1
FLAG_HAS_LIBMAIN = 0x2
FLAG_HAS_CRC = 0x4
_KNOWN_FILE_FLAGS = FLAG_HAS_ENTRY | FLAG_HAS_LIBMAIN | FLAG_HAS_CRC

SEC_EXEC = 0x01
SEC_WRITE = 0x02
SEC_READ = 0x04
SEC_METADATA = 0x10           # file-only, never mapped
SEC_RELRO = 0x20              # writable while loading, read-only afterwards
_KNOWN_SEC_FLAGS = SEC_EXEC | SEC_WRITE | SEC_READ | SEC_METADATA | SEC_RELRO

RELOC_ABS64 = 1               # u64 = image_base + T
RELOC_ABS32 = 2               # u32 = image_base + T   (image must be < 4 GiB)
RELOC_REL32 = 3               # i32 = T - (patch_rva + 4)   (base independent)
RELOC_RVA32 = 4               # u32 = T                     (base independent)
RELOC_NAMES = {RELOC_ABS64: "ABS64", RELOC_ABS32: "ABS32",
               RELOC_REL32: "REL32", RELOC_RVA32: "RVA32"}
RELOC_WIDTH = {RELOC_ABS64: 8, RELOC_ABS32: 4, RELOC_REL32: 4, RELOC_RVA32: 4}
TARGET_IMAGE = 0xFF           # reloc target "section" meaning: RVA 0 (image)

IMP_BY_ORDINAL = 0x1

# ---- kernel (LKF) header extension
KF_FIXED_ADDR = 0x1           # must be mapped exactly at load_addr (loader fails otherwise)
KF_SMP = 0x2                  # kernel is SMP-aware
KF_DEBUG = 0x4                # debug build
_KNOWN_KERNEL_FLAGS = KF_FIXED_ADDR | KF_SMP | KF_DEBUG
BOOT_LORD = 1                 # entry(u64 BOOT_MAGIC, struct bootinfo *)
BOOT_RAW = 2                  # entry() with rdi = rsi = 0
BOOT_PROTOS = {BOOT_LORD: "lordboot", BOOT_RAW: "raw"}
BOOT_MAGIC = 0x4C4F5244       # "LORD"
BOOTINFO_VERSION = 1

# ---- driver (LDF) header extension
DRV_CHAR, DRV_BLOCK, DRV_NET, DRV_INPUT = 1, 2, 3, 4
DRV_DISPLAY, DRV_BUS, DRV_FS, DRV_GENERIC = 5, 6, 7, 8
DRV_CLASSES = {DRV_CHAR: "char", DRV_BLOCK: "block", DRV_NET: "net",
               DRV_INPUT: "input", DRV_DISPLAY: "display", DRV_BUS: "bus",
               DRV_FS: "fs", DRV_GENERIC: "generic"}
_DF_HAS_PROBE = 0x01          # wire-only presence bits, derived from the model
_DF_HAS_REMOVE = 0x02
DF_HOTPLUG = 0x04             # device may appear/disappear at runtime
DF_DMA = 0x08                 # driver does DMA
DF_IRQ = 0x10                 # driver services interrupts
DF_PERSISTENT = 0x20          # may not be unloaded once loaded
DRV_FLAG_NAMES = {"hotplug": DF_HOTPLUG, "dma": DF_DMA, "irq": DF_IRQ,
                  "persistent": DF_PERSISTENT}
_KNOWN_DRIVER_FLAGS = (_DF_HAS_PROBE | _DF_HAS_REMOVE | DF_HOTPLUG | DF_DMA
                       | DF_IRQ | DF_PERSISTENT)
ANY_ID = 0xFFFF               # wildcard in a device-id entry

DEFAULT_FILE_ALIGN = 16       # use 4096 if sections should be mmap()-able
DEFAULT_SECTION_ALIGN = 0x1000
MIN_SECTION_ALIGN = 0x1000
MAX_SECTIONS = 254            # index 0xFF is reserved (TARGET_IMAGE)
IAT_SLOT_SIZE = 8

DEVTAB = b".devtab"
STRTAB = b".strtab"
EDATA = b".edata"
IMPDIR = b".impdir"
RELOC = b".reloc"
SYMTAB = b".symtab"
IDATA = b".idata"
GENERATED = (STRTAB, EDATA, IMPDIR, RELOC, SYMTAB, DEVTAB)

FILEHDR_FMT = "<4s4H9IQ2HI"
SECHDR_FMT = "<8s5I"
EXPORT_FMT = "<IB3xIII"       # name_off section offset size ordinal
IMPDESC_FMT = "<II"           # lib_name_off count
IMPENT_FMT = "<IIII"          # name_off ordinal iat_rva flags
RELOC_FMT = "<IBBHq"          # rva type target_section pad addend
SYMBOL_FMT = "<IB3xI"         # name_off section offset
KERNEL_EXT_FMT = "<6IQ16s"    # kflags boot_proto version kabi cmdline reserved load_addr build_id
DRIVER_EXT_FMT = "<IIHH7I"    # name version class pad flags kabi_min kabi_max license probe remove reserved
DEVID_FMT = "<HHII"           # vendor device class_code class_mask

FILEHDR_SIZE = struct.calcsize(FILEHDR_FMT)
SECHDR_SIZE = struct.calcsize(SECHDR_FMT)
CRC_OFFSET = FILEHDR_SIZE - 4
EXT_SIZE = {TYPE_KERNEL: struct.calcsize(KERNEL_EXT_FMT),
            TYPE_DRIVER: struct.calcsize(DRIVER_EXT_FMT)}
assert FILEHDR_SIZE == 64 and SECHDR_SIZE == 28
assert EXT_SIZE == {TYPE_KERNEL: 48, TYPE_DRIVER: 40}


def header_size_for(filetype):
    """Base header plus the type-specific extension (LKF 112, LDF 104)."""
    return FILEHDR_SIZE + EXT_SIZE.get(filetype, 0)


class LordFormatError(ValueError):
    """The file is malformed or the model cannot be serialised."""


# ------------------------------------------------------------------- helpers
def align_up(v, a):
    return (v + a - 1) // a * a


def _is_pow2(v):
    return v > 0 and (v & (v - 1)) == 0


def _bname(n):
    if isinstance(n, str):
        n = n.encode("ascii")
    n = bytes(n)
    if not 1 <= len(n) <= 8 or any(c <= 0x20 or c >= 0x7F for c in n):
        raise LordFormatError("bad section name %r (1-8 printable ASCII chars)" % n)
    return n


def pack_version(text):
    """'1.2.3' -> (1 << 16) | (2 << 8) | 3   (major u16, minor u8, patch u8)."""
    try:
        parts = [int(p) for p in str(text).split(".")]
    except ValueError:
        parts = []
    if not 1 <= len(parts) <= 3:
        raise LordFormatError("bad version %r (want MAJOR[.MINOR[.PATCH]])" % text)
    major, minor, patch = (parts + [0, 0])[:3]
    if not (0 <= major <= 0xFFFF and 0 <= minor <= 0xFF and 0 <= patch <= 0xFF):
        raise LordFormatError("version %r out of range (65535.255.255)" % text)
    return major << 16 | minor << 8 | patch


def fmt_version(v):
    return "%d.%d.%d" % (v >> 16, v >> 8 & 0xFF, v & 0xFF)


class _StrTab:
    """Builds the .strtab section; offset 0 is always the empty string."""

    def __init__(self):
        self.buf = bytearray(b"\0")
        self.cache = {"": 0}

    def add(self, s):
        if s not in self.cache:
            raw = s.encode("utf-8")
            if b"\0" in raw:
                raise LordFormatError("NUL in string %r" % s)
            self.cache[s] = len(self.buf)
            self.buf += raw + b"\0"
        return self.cache[s]


def _cstr(strtab, off):
    if off == 0:                  # offset 0 is the empty string by definition,
        return ""                 # even when no .strtab was written at all
    if off >= len(strtab):
        raise LordFormatError("string offset %#x outside .strtab" % off)
    end = strtab.find(b"\0", off)
    if end < 0:
        raise LordFormatError("unterminated string at .strtab+%#x" % off)
    try:
        return strtab[off:end].decode("utf-8")
    except UnicodeDecodeError:
        raise LordFormatError("invalid UTF-8 at .strtab+%#x" % off)


# --------------------------------------------------------------------- model
class Section:
    def __init__(self, name, data=b"", flags=SEC_READ, virtual_size=0):
        self.name = _bname(name)
        self.data = bytes(data)
        self.flags = flags
        self.virtual_size = virtual_size
        self.rva = 0              # filled in by to_bytes()/from_bytes()
        self.file_offset = 0

    @property
    def mapped(self):
        return not (self.flags & SEC_METADATA)

    @property
    def mem_size(self):
        if self.virtual_size and self.virtual_size < len(self.data):
            raise LordFormatError("%s: virtual_size smaller than file data"
                                  % self.name.decode())
        return self.virtual_size or len(self.data)

    def __repr__(self):
        return "Section(%s, %d bytes, flags=%#x)" % (
            self.name.decode(), len(self.data), self.flags)


@dataclass
class Export:
    name: str
    section: bytes
    offset: int
    size: int = 0
    ordinal: int = 0              # 0 = assign automatically

    def __post_init__(self):
        self.section = _bname(self.section)


@dataclass
class Import:
    lib: str
    func: str = None              # None => import by ordinal
    ordinal: int = 0
    iat_rva: int = 0              # filled in by to_bytes()/from_bytes()


@dataclass
class Reloc:
    section: bytes                # section containing the patch location
    offset: int                   # offset of the patch location in it
    type: int
    target: bytes = None          # target section; None => image (RVA 0)
    addend: int = 0               # T = target.rva + addend

    def __post_init__(self):
        self.section = _bname(self.section)
        if self.target is not None:
            self.target = _bname(self.target)


@dataclass
class Symbol:
    name: str
    section: bytes
    offset: int

    def __post_init__(self):
        self.section = _bname(self.section)


@dataclass
class KernelInfo:
    """LKF header extension."""
    version: int = 0              # see pack_version()
    kabi: int = 1                 # kernel ABI level that drivers are checked against
    boot_proto: int = BOOT_LORD
    flags: int = 0                # KF_*
    cmdline: str = ""             # default kernel command line
    load_addr: int = 0            # where the kernel wants to live (0 = anywhere)
    build_id: bytes = bytes(16)


@dataclass
class DriverInfo:
    """LDF header extension. probe/remove are (section, offset) or None."""
    name: str = ""
    version: int = 0
    dclass: int = DRV_GENERIC
    flags: int = 0                # DF_* (presence bits are derived)
    kabi_min: int = 1             # kernel ABI range this driver works with
    kabi_max: int = 0             # 0 = no upper bound
    license: str = ""
    probe: tuple = None           # int probe(u32 vendor, u32 device, u32 class)
    remove: tuple = None          # void remove(void)


@dataclass
class DeviceId:
    """One .devtab row (PCI-style). ANY_ID is a wildcard for vendor/device;
    class matching is `(class ^ class_code) & class_mask == 0`."""
    vendor: int = ANY_ID
    device: int = ANY_ID
    class_code: int = 0
    class_mask: int = 0


class LordFile:
    def __init__(self, filetype=TYPE_EXECUTABLE, arch=ARCH_X86_64):
        self.filetype = filetype
        self.arch = arch
        self.sections = []        # Section objects
        self.exports = []
        self.imports = []         # slot N of .idata belongs to imports[N]
        self.relocs = []
        self.symbols = []
        self.entry = None         # (section_name, offset) or None
        self.libmain = None       # (section_name, offset) or None
        self.stack_size = 0       # 0 = host default
        self.preferred_base = 0   # hint only, 0 = anywhere
        self.min_loader = (KERNEL_MIN_LOADER
                           if filetype in (TYPE_KERNEL, TYPE_DRIVER) else 2)
        self.kernel = KernelInfo() if filetype == TYPE_KERNEL else None
        self.driver = DriverInfo() if filetype == TYPE_DRIVER else None
        self.devices = []         # DeviceId rows (drivers only)
        self.file_align = DEFAULT_FILE_ALIGN
        self.section_align = DEFAULT_SECTION_ALIGN
        self.with_crc = True

    # ------------------------------------------------------------- building
    @property
    def is_library(self):
        return self.filetype == TYPE_LIBRARY

    @property
    def is_kernel(self):
        return self.filetype == TYPE_KERNEL

    @property
    def is_driver(self):
        return self.filetype == TYPE_DRIVER

    def match_device(self, vendor, device, class_code=0):
        """First .devtab row matching this device, or None."""
        for d in self.devices:
            if d.vendor not in (ANY_ID, vendor) or d.device not in (ANY_ID, device):
                continue
            if (class_code ^ d.class_code) & d.class_mask:
                continue
            return d
        return None

    def add_section(self, name, data=b"", flags=SEC_READ, virtual_size=0):
        s = Section(name, data, flags, virtual_size)
        self.sections.append(s)
        return s

    def section(self, name):
        name = _bname(name)
        for s in self.sections:
            if s.name == name:
                return s
        raise LordFormatError("no section %s" % name.decode())

    def add_import(self, lib, func=None, ordinal=0):
        """Return the IAT slot index for (lib, func/ordinal), reusing slots."""
        if func is None and ordinal <= 0:
            raise LordFormatError("import needs a name or an ordinal > 0")
        for i, imp in enumerate(self.imports):
            if imp.lib == lib and imp.func == func and imp.ordinal == ordinal:
                return i
        self.imports.append(Import(lib, func, ordinal))
        return len(self.imports) - 1

    def find_export(self, name):
        """Binary search; the export table is kept sorted by name."""
        self.exports.sort(key=lambda e: e.name.encode("utf-8"))
        keys = [e.name.encode("utf-8") for e in self.exports]
        key = name.encode("utf-8")
        i = bisect_left(keys, key)
        return self.exports[i] if i < len(keys) and keys[i] == key else None

    def find_export_ordinal(self, ordinal):
        for e in self.exports:
            if e.ordinal == ordinal:
                return e
        return None

    def rva_of(self, section, offset):
        return self.section(section).rva + offset

    def locate(self, rva):
        """Map an RVA back to (section_name, offset) or None."""
        for s in self.sections:
            if s.mapped and s.rva <= rva < s.rva + s.mem_size:
                return s.name, rva - s.rva
        return None

    # ---------------------------------------------------------- serialising
    def to_bytes(self):
        if self.filetype not in MAGIC_BY_TYPE:
            raise LordFormatError("bad filetype")
        if self.kernel is not None and self.filetype != TYPE_KERNEL:
            raise LordFormatError("kernel info on a non-kernel file")
        if self.filetype == TYPE_KERNEL and self.kernel is None:
            raise LordFormatError("kernel file needs kernel info")
        if self.driver is not None and self.filetype != TYPE_DRIVER:
            raise LordFormatError("driver info on a non-driver file")
        if self.filetype == TYPE_DRIVER and self.driver is None:
            raise LordFormatError("driver file needs driver info")
        if self.devices and self.filetype != TYPE_DRIVER:
            raise LordFormatError("device ids only make sense in drivers")
        if self.filetype in (TYPE_KERNEL, TYPE_DRIVER) and self.min_loader < KERNEL_MIN_LOADER:
            raise LordFormatError("kernel/driver files need min_loader >= %d"
                                  % KERNEL_MIN_LOADER)
        if not (_is_pow2(self.file_align) and _is_pow2(self.section_align)
                and self.section_align >= MIN_SECTION_ALIGN):
            raise LordFormatError("bad alignment")
        if self.preferred_base % self.section_align:
            raise LordFormatError("preferred_base not section-aligned")

        mapped, extra = [], []
        for s in self.sections:
            if s.name in GENERATED:
                continue
            if s.name == IDATA and self.imports:
                # A parsed file carries its generated .idata (zeroed, RW+RELRO);
                # drop that one and regenerate. Anything else would be ambiguous.
                if s.flags == SEC_READ | SEC_WRITE | SEC_RELRO and not any(s.data):
                    continue
                raise LordFormatError(".idata is generated when imports exist")
            if s.flags & ~_KNOWN_SEC_FLAGS:
                raise LordFormatError("%s: unknown flags" % s.name.decode())
            (mapped if s.mapped else extra).append(s)
        if self.imports:
            mapped.append(Section(IDATA, bytes(IAT_SLOT_SIZE * len(self.imports)),
                                  SEC_READ | SEC_WRITE | SEC_RELRO))
        for s in mapped:
            if s.flags & SEC_EXEC and s.flags & SEC_WRITE:
                raise LordFormatError("%s: W^X violation (EXEC and WRITE)"
                                      % s.name.decode())
            if s.mem_size == 0:
                raise LordFormatError("%s: empty mapped section" % s.name.decode())
        names = [s.name for s in mapped + extra]
        if len(set(names)) != len(names):
            raise LordFormatError("duplicate section names")

        cursor = self.section_align
        for s in mapped:
            s.rva = cursor
            cursor = align_up(cursor + s.mem_size, self.section_align)
        if cursor >= 1 << 32:
            raise LordFormatError("image too large")
        index = {s.name: i for i, s in enumerate(mapped)}

        def sidx(name, what):
            if name not in index:
                raise LordFormatError("%s: unknown or unmapped section %s"
                                      % (what, name.decode()))
            return index[name]

        def check_range(name, off, width, what):
            s = mapped[sidx(name, what)]
            if off < 0 or off + width > s.mem_size:
                raise LordFormatError("%s: %s+%#x out of range"
                                      % (what, name.decode(), off))
            return s

        st = _StrTab()

        # exports (sorted by name, ordinals unique)
        exports = sorted(self.exports, key=lambda e: e.name.encode("utf-8"))
        used = {e.ordinal for e in exports if e.ordinal}
        if len(used) != sum(1 for e in exports if e.ordinal):
            raise LordFormatError("duplicate export ordinals")
        nxt = 1
        for e in exports:
            if not e.ordinal:
                while nxt in used:
                    nxt += 1
                e.ordinal = nxt
                used.add(nxt)
        if len({e.name for e in exports}) != len(exports):
            raise LordFormatError("duplicate export names")
        edata = b""
        for e in exports:
            check_range(e.section, e.offset, 1, "export " + e.name)
            edata += struct.pack(EXPORT_FMT, st.add(e.name),
                                 sidx(e.section, "export"), e.offset, e.size,
                                 e.ordinal)
        self.exports = exports

        # imports, grouped per library (slot = position in self.imports)
        idata = mapped[index[IDATA]] if self.imports else None
        groups = {}
        for slot, imp in enumerate(self.imports):
            imp.iat_rva = idata.rva + IAT_SLOT_SIZE * slot
            groups.setdefault(imp.lib, []).append(imp)
        impdir = b""
        for lib, imps in groups.items():
            impdir += struct.pack(IMPDESC_FMT, st.add(lib), len(imps))
            for imp in imps:
                by_ord = imp.func is None
                impdir += struct.pack(IMPENT_FMT,
                                      0 if by_ord else st.add(imp.func),
                                      imp.ordinal if by_ord else 0,
                                      imp.iat_rva,
                                      IMP_BY_ORDINAL if by_ord else 0)

        # relocations
        reloc = b""
        for r in self.relocs:
            if r.type not in RELOC_WIDTH:
                raise LordFormatError("bad relocation type %r" % r.type)
            s = check_range(r.section, r.offset, RELOC_WIDTH[r.type],
                            "reloc")
            tgt = TARGET_IMAGE if r.target is None else sidx(r.target, "reloc target")
            reloc += struct.pack(RELOC_FMT, s.rva + r.offset, r.type, tgt, 0,
                                 r.addend)

        # symbols (debug info)
        symtab = b""
        for sy in self.symbols:
            check_range(sy.section, sy.offset, 0, "symbol " + sy.name)
            symtab += struct.pack(SYMBOL_FMT, st.add(sy.name),
                                  sidx(sy.section, "symbol"), sy.offset)

        # entry points
        flags = FLAG_HAS_CRC if self.with_crc else 0
        entry_rva = libmain_rva = 0
        if self.entry is not None:
            if self.filetype not in ENTRY_TYPES:
                raise LordFormatError("only executables and kernels have an entry point")
            sec = check_range(_bname(self.entry[0]), self.entry[1], 1, "entry")
            if not sec.flags & SEC_EXEC:
                raise LordFormatError("entry not in an executable section")
            entry_rva, flags = sec.rva + self.entry[1], flags | FLAG_HAS_ENTRY
        elif self.filetype in ENTRY_TYPES:
            raise LordFormatError("%s needs an entry point" % TYPE_LABEL[self.filetype])
        if self.libmain is not None:
            sec = check_range(_bname(self.libmain[0]), self.libmain[1], 1, "libmain")
            if not sec.flags & SEC_EXEC:
                raise LordFormatError("libmain not in an executable section")
            libmain_rva, flags = sec.rva + self.libmain[1], flags | FLAG_HAS_LIBMAIN

        # type-specific header extension (+ .devtab for drivers)
        ext, devtab = b"", b""
        if self.filetype == TYPE_KERNEL:
            k = self.kernel
            if k.flags & ~_KNOWN_KERNEL_FLAGS:
                raise LordFormatError("unknown kernel flags %#x" % k.flags)
            if k.boot_proto not in BOOT_PROTOS:
                raise LordFormatError("unknown boot protocol %r" % k.boot_proto)
            if not 1 <= k.kabi < 1 << 32 or not 0 <= k.version < 1 << 32:
                raise LordFormatError("kernel kabi must be >= 1, version a u32")
            if k.load_addr % self.section_align or not 0 <= k.load_addr < 1 << 64:
                raise LordFormatError("load_addr not section-aligned")
            if k.flags & KF_FIXED_ADDR and not k.load_addr:
                raise LordFormatError("FIXED_ADDR needs a load_addr")
            if len(k.build_id) != 16:
                raise LordFormatError("build_id must be 16 bytes")
            ext = struct.pack(KERNEL_EXT_FMT, k.flags, k.boot_proto, k.version,
                              k.kabi, st.add(k.cmdline), 0, k.load_addr,
                              bytes(k.build_id))
        elif self.filetype == TYPE_DRIVER:
            d = self.driver
            if not d.name:
                raise LordFormatError("driver needs a name")
            if d.dclass not in DRV_CLASSES:
                raise LordFormatError("unknown driver class %r" % d.dclass)
            if d.flags & ~_KNOWN_DRIVER_FLAGS or d.flags & (_DF_HAS_PROBE | _DF_HAS_REMOVE):
                raise LordFormatError("bad driver flags %#x" % d.flags)
            if not 0 <= d.version < 1 << 32:
                raise LordFormatError("bad driver version")
            if not 1 <= d.kabi_min < 1 << 32 or (d.kabi_max and d.kabi_max < d.kabi_min):
                raise LordFormatError("bad kabi range %d..%d" % (d.kabi_min, d.kabi_max))
            dflags, hooks = d.flags, []
            for bit, hook, what in ((_DF_HAS_PROBE, d.probe, "probe"),
                                    (_DF_HAS_REMOVE, d.remove, "remove")):
                rva = 0
                if hook is not None:
                    sec = check_range(_bname(hook[0]), hook[1], 1, what)
                    if not sec.flags & SEC_EXEC:
                        raise LordFormatError("%s not in an executable section" % what)
                    rva, dflags = sec.rva + hook[1], dflags | bit
                hooks.append(rva)
            if self.devices and d.probe is None:
                raise LordFormatError("a driver with device ids needs a probe function")
            ext = struct.pack(DRIVER_EXT_FMT, st.add(d.name), d.version, d.dclass,
                              0, dflags, d.kabi_min, d.kabi_max, st.add(d.license),
                              hooks[0], hooks[1], 0)
            for dev in self.devices:
                if not (0 <= dev.vendor <= 0xFFFF and 0 <= dev.device <= 0xFFFF
                        and 0 <= dev.class_code <= 0xFFFFFF
                        and 0 <= dev.class_mask <= 0xFFFFFF):
                    raise LordFormatError("device id field out of range")
                if dev.vendor == ANY_ID and dev.device != ANY_ID:
                    raise LordFormatError("device id with a device but no vendor")
                devtab += struct.pack(DEVID_FMT, dev.vendor, dev.device,
                                      dev.class_code, dev.class_mask)
        assert len(ext) == EXT_SIZE.get(self.filetype, 0)
        hsize = FILEHDR_SIZE + len(ext)

        meta = []
        for name, blob in ((STRTAB, bytes(st.buf) if len(st.buf) > 1 else b""),
                           (EDATA, edata), (IMPDIR, impdir),
                           (RELOC, reloc), (SYMTAB, symtab), (DEVTAB, devtab)):
            if blob:
                meta.append(Section(name, blob, SEC_METADATA))
        final = mapped + meta + extra
        if len(final) > MAX_SECTIONS:
            raise LordFormatError("too many sections")

        cursor = hsize + SECHDR_SIZE * len(final)
        for s in final:
            if s.data:
                cursor = align_up(cursor, self.file_align)
                s.file_offset = cursor
                cursor += len(s.data)
            else:
                s.file_offset = 0
        file_size = cursor
        if file_size >= 1 << 32:
            raise LordFormatError("file too large")

        buf = bytearray(file_size)
        magic = MAGIC_BY_TYPE[self.filetype]
        struct.pack_into(FILEHDR_FMT, buf, 0, magic, FORMAT_VERSION, self.arch,
                         self.filetype, len(final), hsize, hsize,
                         file_size, self.file_align, self.section_align,
                         entry_rva, libmain_rva, flags, self.stack_size,
                         self.preferred_base, self.min_loader, 0, 0)
        for i, s in enumerate(final):
            struct.pack_into(SECHDR_FMT, buf, hsize + i * SECHDR_SIZE,
                             s.name, s.file_offset, len(s.data),
                             s.rva if s.mapped else 0,
                             s.mem_size if s.mapped else 0, s.flags)
            buf[s.file_offset:s.file_offset + len(s.data)] = s.data
        buf[FILEHDR_SIZE:hsize] = ext
        if self.with_crc:
            struct.pack_into("<I", buf, CRC_OFFSET, zlib.crc32(bytes(buf)))
        return bytes(buf)

    # ------------------------------------------------------------- parsing
    @classmethod
    def from_bytes(cls, buf):
        buf = bytes(buf)
        if len(buf) < FILEHDR_SIZE:
            raise LordFormatError("file too short for header")
        (magic, version, arch, filetype, nsec, header_size, table_off,
         file_size, file_align, section_align, entry_rva, libmain_rva, flags,
         stack_size, preferred_base, min_loader, _res,
         crc) = struct.unpack_from(FILEHDR_FMT, buf, 0)

        want = TYPE_BY_MAGIC.get(magic)
        if want is None:
            raise LordFormatError("bad magic %r" % magic)
        if filetype != want:
            raise LordFormatError("magic/filetype mismatch")
        if version != FORMAT_VERSION:
            raise LordFormatError("unsupported format version %d (this code "
                                  "reads v%d; rebuild the file)"
                                  % (version, FORMAT_VERSION))
        if min_loader > LOADER_VERSION:
            raise LordFormatError("file needs loader >= %d, this is %d"
                                  % (min_loader, LOADER_VERSION))
        if header_size != header_size_for(filetype):
            raise LordFormatError("bad header_size")
        if filetype in (TYPE_KERNEL, TYPE_DRIVER) and min_loader < KERNEL_MIN_LOADER:
            raise LordFormatError("kernel/driver files need min_loader >= %d"
                                  % KERNEL_MIN_LOADER)
        if file_size != len(buf):
            raise LordFormatError("file_size %d != actual size %d"
                                  % (file_size, len(buf)))
        if flags & ~_KNOWN_FILE_FLAGS:
            raise LordFormatError("unknown header flags %#x" % flags)
        if not (_is_pow2(file_align) and _is_pow2(section_align)
                and section_align >= MIN_SECTION_ALIGN):
            raise LordFormatError("bad alignment fields")
        if preferred_base % section_align:
            raise LordFormatError("preferred_base not section-aligned")
        if not flags & FLAG_HAS_CRC and crc != 0:
            raise LordFormatError("CRC field set but HAS_CRC flag clear")
        if flags & FLAG_HAS_CRC:
            tmp = bytearray(buf)
            tmp[CRC_OFFSET:CRC_OFFSET + 4] = b"\0\0\0\0"
            if zlib.crc32(bytes(tmp)) != crc:
                raise LordFormatError("CRC32 mismatch (file corrupted)")
        if nsec > MAX_SECTIONS:
            raise LordFormatError("too many sections")
        if table_off < header_size or table_off + nsec * SECHDR_SIZE > file_size:
            raise LordFormatError("section table outside file")

        lf = cls(filetype, arch)
        lf.stack_size, lf.preferred_base = stack_size, preferred_base
        lf.min_loader = min_loader
        lf.file_align, lf.section_align = file_align, section_align
        lf.with_crc = bool(flags & FLAG_HAS_CRC)

        file_ranges = [(0, table_off + nsec * SECHDR_SIZE, "headers")]
        ext_raw = buf[FILEHDR_SIZE:header_size]
        mem_ranges = []
        for i in range(nsec):
            (raw, foff, fsize, rva, vsize,
             sflags) = struct.unpack_from(SECHDR_FMT, buf,
                                          table_off + i * SECHDR_SIZE)
            name = _bname(raw.rstrip(b"\0"))
            if sflags & ~_KNOWN_SEC_FLAGS:
                raise LordFormatError("%s: unknown section flags" % name.decode())
            if foff + fsize > file_size:
                raise LordFormatError("%s: data outside file" % name.decode())
            if fsize:
                if foff % file_align:
                    raise LordFormatError("%s: misaligned file offset"
                                          % name.decode())
                file_ranges.append((foff, foff + fsize, name.decode()))
            elif foff:
                raise LordFormatError("%s: empty section with offset"
                                      % name.decode())
            s = Section(name, buf[foff:foff + fsize], sflags, vsize)
            s.file_offset = foff
            if sflags & SEC_METADATA:
                if sflags & (SEC_EXEC | SEC_WRITE | SEC_RELRO) or rva or vsize:
                    raise LordFormatError("%s: bad metadata section"
                                          % name.decode())
            else:
                if sflags & SEC_EXEC and sflags & SEC_WRITE:
                    raise LordFormatError("%s: W^X violation" % name.decode())
                if rva % section_align:
                    raise LordFormatError("%s: misaligned rva" % name.decode())
                if vsize < fsize or vsize == 0:
                    raise LordFormatError("%s: bad virtual_size" % name.decode())
                if rva + vsize >= 1 << 32:
                    raise LordFormatError("%s: rva out of range" % name.decode())
                s.rva = rva
                mem_ranges.append((rva, rva + vsize, name.decode()))
            lf.sections.append(s)
        if len({s.name for s in lf.sections}) != nsec:
            raise LordFormatError("duplicate section names")
        for ranges, what in ((file_ranges, "file"), (mem_ranges, "memory")):
            ranges.sort()
            for a, b in zip(ranges, ranges[1:]):
                if b[0] < a[1]:
                    raise LordFormatError("%s ranges overlap: %s / %s"
                                          % (what, a[2], b[2]))

        mapped = [s for s in lf.sections if s.mapped]
        mindex = {s.name: i for i, s in enumerate(lf.sections) if s.mapped}
        by_index = {i: s for i, s in enumerate(lf.sections) if s.mapped}

        def meta(name):
            for s in lf.sections:
                if s.name == name:
                    if s.mapped:
                        raise LordFormatError("%s must be a metadata section"
                                              % name.decode())
                    return s.data
            return b""

        strtab = meta(STRTAB)

        def sec_by_idx(i, what):
            if i not in by_index:
                raise LordFormatError("%s: bad section index %d" % (what, i))
            return by_index[i]

        def in_range(sec, off, width, what):
            if off + width > sec.mem_size:
                raise LordFormatError("%s: offset %#x out of range in %s"
                                      % (what, off, sec.name.decode()))

        # exports
        data = meta(EDATA)
        esz = struct.calcsize(EXPORT_FMT)
        if len(data) % esz:
            raise LordFormatError("bad .edata size")
        prev, ords = None, set()
        for off in range(0, len(data), esz):
            name_off, si, o, size, ordinal = struct.unpack_from(EXPORT_FMT, data, off)
            name = _cstr(strtab, name_off)
            sec = sec_by_idx(si, "export " + name)
            in_range(sec, o, 1, "export " + name)
            key = name.encode("utf-8")
            if prev is not None and key <= prev:
                raise LordFormatError(".edata not strictly sorted by name")
            if ordinal == 0 or ordinal in ords:
                raise LordFormatError("bad/duplicate export ordinal")
            prev = key
            ords.add(ordinal)
            lf.exports.append(Export(name, sec.name, o, size, ordinal))

        # imports
        data = meta(IMPDIR)
        pos, imps, seen = 0, [], set()
        dsz, isz = struct.calcsize(IMPDESC_FMT), struct.calcsize(IMPENT_FMT)
        while pos < len(data):
            if pos + dsz > len(data):
                raise LordFormatError("truncated .impdir")
            lib_off, count = struct.unpack_from(IMPDESC_FMT, data, pos)
            pos += dsz
            lib = _cstr(strtab, lib_off)
            if not lib or pos + count * isz > len(data):
                raise LordFormatError("bad import descriptor")
            for _ in range(count):
                noff, ordinal, iat, iflags = struct.unpack_from(IMPENT_FMT, data, pos)
                pos += isz
                if iflags & ~IMP_BY_ORDINAL:
                    raise LordFormatError("unknown import flags")
                if iflags & IMP_BY_ORDINAL:
                    if ordinal == 0:
                        raise LordFormatError("ordinal import with ordinal 0")
                    imp = Import(lib, None, ordinal, iat)
                else:
                    imp = Import(lib, _cstr(strtab, noff), 0, iat)
                    if not imp.func:
                        raise LordFormatError("empty import name")
                holder = next((s for s in mapped if s.rva <= iat
                               and iat + IAT_SLOT_SIZE <= s.rva + s.mem_size), None)
                if holder is None or not holder.flags & SEC_WRITE:
                    raise LordFormatError("IAT slot %#x not in a writable section"
                                          % iat)
                if iat % IAT_SLOT_SIZE or iat in seen:
                    raise LordFormatError("misaligned/duplicate IAT slot")
                seen.add(iat)
                imps.append(imp)
        lf.imports = sorted(imps, key=lambda i: i.iat_rva)

        # relocations
        data = meta(RELOC)
        rsz = struct.calcsize(RELOC_FMT)
        if len(data) % rsz:
            raise LordFormatError("bad .reloc size")
        for off in range(0, len(data), rsz):
            rva, typ, tgt, _pad, addend = struct.unpack_from(RELOC_FMT, data, off)
            if typ not in RELOC_WIDTH:
                raise LordFormatError("unknown relocation type %d" % typ)
            where = lf.locate(rva)
            if where is None or lf.section(where[0]).mem_size < where[1] + RELOC_WIDTH[typ]:
                raise LordFormatError("relocation at %#x outside image" % rva)
            target = None if tgt == TARGET_IMAGE else sec_by_idx(tgt, "reloc").name
            lf.relocs.append(Reloc(where[0], where[1], typ, target, addend))

        # symbols
        data = meta(SYMTAB)
        ssz = struct.calcsize(SYMBOL_FMT)
        if len(data) % ssz:
            raise LordFormatError("bad .symtab size")
        for off in range(0, len(data), ssz):
            noff, si, o = struct.unpack_from(SYMBOL_FMT, data, off)
            sec = sec_by_idx(si, "symbol")
            in_range(sec, o, 0, "symbol")
            lf.symbols.append(Symbol(_cstr(strtab, noff), sec.name, o))

        # type-specific extension
        devtab = meta(DEVTAB)
        if devtab and filetype != TYPE_DRIVER:
            raise LordFormatError(".devtab in a non-driver file")
        if filetype == TYPE_KERNEL:
            (kflags, proto, kver, kabi, cmd_off, kres, load_addr,
             build_id) = struct.unpack(KERNEL_EXT_FMT, ext_raw)
            if kflags & ~_KNOWN_KERNEL_FLAGS:
                raise LordFormatError("unknown kernel flags %#x" % kflags)
            if proto not in BOOT_PROTOS:
                raise LordFormatError("unknown boot protocol %d" % proto)
            if kabi < 1:
                raise LordFormatError("kernel kabi must be >= 1")
            if kres:
                raise LordFormatError("kernel header reserved field set")
            if load_addr % section_align:
                raise LordFormatError("load_addr not section-aligned")
            if kflags & KF_FIXED_ADDR and not load_addr:
                raise LordFormatError("FIXED_ADDR without load_addr")
            lf.kernel = KernelInfo(kver, kabi, proto, kflags,
                                   _cstr(strtab, cmd_off), load_addr, build_id)
        elif filetype == TYPE_DRIVER:
            (name_off, dver, dclass, dpad, dflags, kmin, kmax, lic_off,
             probe_rva, remove_rva, dres) = struct.unpack(DRIVER_EXT_FMT, ext_raw)
            if dflags & ~_KNOWN_DRIVER_FLAGS:
                raise LordFormatError("unknown driver flags %#x" % dflags)
            if dclass not in DRV_CLASSES:
                raise LordFormatError("unknown driver class %d" % dclass)
            if dpad or dres:
                raise LordFormatError("driver header reserved field set")
            if kmin < 1 or (kmax and kmax < kmin):
                raise LordFormatError("bad kabi range %d..%d" % (kmin, kmax))
            dname = _cstr(strtab, name_off)
            if not dname:
                raise LordFormatError("driver without a name")
            hooks = []
            for bit, rva, what in ((_DF_HAS_PROBE, probe_rva, "probe"),
                                   (_DF_HAS_REMOVE, remove_rva, "remove")):
                if dflags & bit:
                    where = lf.locate(rva)
                    if where is None or not lf.section(where[0]).flags & SEC_EXEC:
                        raise LordFormatError("%s not in an executable section" % what)
                    hooks.append(where)
                else:
                    if rva:
                        raise LordFormatError("%s address without its flag" % what)
                    hooks.append(None)
            dsz = struct.calcsize(DEVID_FMT)
            if len(devtab) % dsz:
                raise LordFormatError("bad .devtab size")
            for off in range(0, len(devtab), dsz):
                ven, dev, ccode, cmask = struct.unpack_from(DEVID_FMT, devtab, off)
                if ccode > 0xFFFFFF or cmask > 0xFFFFFF:
                    raise LordFormatError("device class field exceeds 24 bits")
                if ven == ANY_ID and dev != ANY_ID:
                    raise LordFormatError("device id with a device but no vendor")
                lf.devices.append(DeviceId(ven, dev, ccode, cmask))
            if lf.devices and hooks[0] is None:
                raise LordFormatError("device ids without a probe function")
            lf.driver = DriverInfo(dname, dver, dclass,
                                   dflags & ~(_DF_HAS_PROBE | _DF_HAS_REMOVE),
                                   kmin, kmax, _cstr(strtab, lic_off),
                                   hooks[0], hooks[1])

        # entry points
        def entry_of(rva, what):
            where = lf.locate(rva)
            if where is None or not lf.section(where[0]).flags & SEC_EXEC:
                raise LordFormatError("%s not in an executable section" % what)
            return where

        if flags & FLAG_HAS_ENTRY:
            if filetype not in ENTRY_TYPES:
                raise LordFormatError("%s with entry point" % TYPE_LABEL[filetype])
            lf.entry = entry_of(entry_rva, "entry")
        elif filetype in ENTRY_TYPES:
            raise LordFormatError("%s without entry point" % TYPE_LABEL[filetype])
        if flags & FLAG_HAS_LIBMAIN:
            lf.libmain = entry_of(libmain_rva, "libmain")
        return lf

    # ------------------------------------------------------------- display
    def describe(self):
        out = ["%s, format v%d, x86-64" % (TYPE_LABEL[self.filetype], FORMAT_VERSION)]
        if self.kernel:
            k = self.kernel
            kf = ",".join(n for n, b in (("fixed_addr", KF_FIXED_ADDR), ("smp", KF_SMP),
                                          ("debug", KF_DEBUG)) if k.flags & b)
            out.append("  kernel:  version %s  kabi %d  boot=%s  flags=%s"
                       % (fmt_version(k.version), k.kabi, BOOT_PROTOS[k.boot_proto],
                          kf or "-"))
            out.append("           load_addr=%#x  build_id=%s"
                       % (k.load_addr, k.build_id.hex()))
            if k.cmdline:
                out.append("           cmdline=%r" % k.cmdline)
        if self.driver:
            d = self.driver
            df = ",".join(n for n, b in DRV_FLAG_NAMES.items() if d.flags & b)
            out.append("  driver:  %s %s  class=%s  flags=%s  license=%s"
                       % (d.name, fmt_version(d.version), DRV_CLASSES[d.dclass],
                          df or "-", d.license or "-"))
            out.append("           needs kabi %d..%s"
                       % (d.kabi_min, d.kabi_max or "any"))
            for what, hook in (("probe", d.probe), ("remove", d.remove)):
                if hook:
                    out.append("           %s:  %s+%#x" % (what, hook[0].decode(), hook[1]))
        out.append("  align: file=%d section=%#x  stack_size=%s  preferred_base=%#x  crc=%s"
                   % (self.file_align, self.section_align,
                      self.stack_size or "default", self.preferred_base,
                      "yes" if self.with_crc else "no"))
        if self.entry:
            out.append("  entry:   %s+%#x" % (self.entry[0].decode(), self.entry[1]))
        if self.libmain:
            out.append("  libmain: %s+%#x" % (self.libmain[0].decode(), self.libmain[1]))
        out.append("  sections:")
        for s in self.sections:
            fl = "".join(c if s.flags & b else "-" for c, b in
                         (("r", SEC_READ), ("w", SEC_WRITE), ("x", SEC_EXEC)))
            fl += "".join(c for c, b in (("M", SEC_METADATA), ("R", SEC_RELRO))
                          if s.flags & b)
            if s.mapped:
                out.append("    %-8s rva=%#07x size=%#06x file=%#06x+%#x  %s"
                           % (s.name.decode(), s.rva, s.mem_size, s.file_offset,
                              len(s.data), fl))
            else:
                out.append("    %-8s %27s file=%#06x+%#x  %s"
                           % (s.name.decode(), "", s.file_offset, len(s.data), fl))
        if self.devices:
            out.append("  devices:")
            for dv in self.devices:
                f = lambda v: "*" if v == ANY_ID else "%04x" % v
                out.append("    %s:%s  class %06x/%06x" % (f(dv.vendor), f(dv.device),
                                                          dv.class_code, dv.class_mask))
        if self.exports:
            out.append("  exports:")
            for e in self.exports:
                out.append("    #%-3d %s  (%s+%#x%s)" % (
                    e.ordinal, e.name, e.section.decode(), e.offset,
                    ", size %d" % e.size if e.size else ""))
        if self.imports:
            out.append("  imports:")
            for i in self.imports:
                what = i.func if i.func is not None else "#%d" % i.ordinal
                out.append("    %s!%s  -> iat %#x" % (i.lib, what, i.iat_rva))
        if self.relocs:
            out.append("  relocations:")
            for r in self.relocs:
                tgt = "image" if r.target is None else r.target.decode()
                out.append("    %s+%#x  %s  -> %s%+d" % (
                    r.section.decode(), r.offset, RELOC_NAMES[r.type], tgt, r.addend))
        if self.symbols:
            out.append("  symbols: %d (use run_lord.py --symbols)" % len(self.symbols))
        return "\n".join(out)


def load_file(path):
    with open(path, "rb") as f:
        return LordFile.from_bytes(f.read())
