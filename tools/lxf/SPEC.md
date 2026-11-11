# Lord formats: LXF / LLF / LKF / LDF, version 2

One container, little-endian, x86-64, four file kinds:

| kind | magic | filetype | header | entry point | purpose |
|---|---|---|---|---|---|
| `LXF` Lord eXecutable Format | `LXF\0` | 1 | 64 B | required | user program |
| `LLF` Lord Library Format    | `LLF\0` | 2 | 64 B | forbidden | shared library |
| `LKF` Lord Kernel Format     | `LKF\0` | 3 | 112 B (64 + 48) | required | kernel image |
| `LDF` Lord Driver Format     | `LDF\0` | 4 | 104 B (64 + 40) | forbidden | kernel-mode driver |

Sections, relocations, imports, exports, W^X and the loading procedure are
identical for all four; the kinds differ in the **header** (LKF and LDF append a
type-specific extension), a few metadata sections, validation rules and ABI.
LXF/LLF files are unchanged from earlier v2 tools, byte for byte.
v1 files are **not** readable (`unsupported format version`); repack them.

## Layout

```
[ file header 64 B ][ type extension (LKF 48 B / LDF 40 B) ][ section table n*28 B ][ section data ... ]
```
`header_size` is 64, 112 or 104 depending on the file kind and must match it
exactly; `section_table_offset` is >= `header_size` (the packer writes it equal).
Section data starts at multiples of `file_align` (default 16; use 4096 if you
want sections to be `mmap`-able straight from the file).

### File header (64 bytes)

| off | size | field | notes |
|---|---|---|---|
| 0  | 4 | magic | `LXF\0`, `LLF\0`, `LKF\0` or `LDF\0`; must agree with `filetype` |
| 4  | 2 | version | 2 |
| 6  | 2 | arch | 1 = x86-64 |
| 8  | 2 | filetype | 1 = executable, 2 = library, 3 = kernel, 4 = driver |
| 10 | 2 | num_sections | at most 254 (index 0xFF is reserved) |
| 12 | 4 | header_size | 64 (LXF/LLF), 112 (LKF), 104 (LDF) |
| 16 | 4 | section_table_offset | >= 64 |
| 20 | 4 | file_size | must equal the real size |
| 24 | 4 | file_align | power of two |
| 28 | 4 | section_align | power of two, >= 0x1000 |
| 32 | 4 | entry_rva | valid if `HAS_ENTRY` |
| 36 | 4 | libmain_rva | valid if `HAS_LIBMAIN` |
| 40 | 4 | flags | `1` HAS_ENTRY, `2` HAS_LIBMAIN, `4` HAS_CRC |
| 44 | 4 | stack_size | 0 = host default; else run the entry on a stack this big |
| 48 | 8 | preferred_base | `mmap` hint only; relocations are always applied |
| 56 | 2 | min_loader | loader refuses the file if it is older than this; LKF/LDF files must say >= 3 |
| 58 | 2 | reserved | 0 |
| 60 | 4 | crc32 | CRC-32 of the whole file with this field zeroed; must be 0 when `HAS_CRC` is clear |

Executables and kernels must have `HAS_ENTRY`; libraries and drivers must not.
Any kind may have `HAS_LIBMAIN`.

### Kernel header extension (LKF, 48 bytes at offset 64)

| off | size | field | notes |
|---|---|---|---|
| 64 | 4 | kflags | `1` FIXED_ADDR, `2` SMP, `4` DEBUG |
| 68 | 4 | boot_proto | `1` lordboot, `2` raw |
| 72 | 4 | version | `major<<16 \| minor<<8 \| patch` (major u16) |
| 76 | 4 | kabi | kernel ABI level, >= 1; drivers are checked against it |
| 80 | 4 | cmdline | `.strtab` offset of the default command line (0 = none) |
| 84 | 4 | reserved | 0 |
| 88 | 8 | load_addr | desired load address, section-aligned; 0 = anywhere |
| 96 | 16 | build_id | opaque identifier (the packer defaults to SHA-256(text)[:16]) |

`FIXED_ADDR` requires a non-zero `load_addr` and obliges the loader to map the
image exactly there, failing otherwise. Without it `load_addr` is a hint (used
when `preferred_base` is 0).

### Driver header extension (LDF, 40 bytes at offset 64)

| off | size | field | notes |
|---|---|---|---|
| 64 | 4 | name | `.strtab` offset; must be non-empty |
| 68 | 4 | version | as for kernels |
| 72 | 2 | class | `1` char `2` block `3` net `4` input `5` display `6` bus `7` fs `8` generic |
| 74 | 2 | pad | 0 |
| 76 | 4 | flags | `1` HAS_PROBE, `2` HAS_REMOVE (derived, must match the addresses below), `4` HOTPLUG, `8` DMA, `0x10` IRQ, `0x20` PERSISTENT |
| 80 | 4 | kabi_min | >= 1 |
| 84 | 4 | kabi_max | 0 = no upper bound, else >= kabi_min |
| 88 | 4 | license | `.strtab` offset (0 = unspecified) |
| 92 | 4 | probe_rva | in an EXEC section if HAS_PROBE, else 0 |
| 96 | 4 | remove_rva | in an EXEC section if HAS_REMOVE, else 0 |
| 100 | 4 | reserved | 0 |

### Section header (28 bytes)

| off | size | field |
|---|---|---|
| 0 | 8 | name (NUL padded, printable ASCII, unique) |
| 8 | 4 | file_offset (0 if the section has no file data) |
| 12 | 4 | file_size |
| 16 | 4 | rva (multiple of `section_align`; 0 for metadata) |
| 20 | 4 | virtual_size (>= file_size; the tail is zero-filled, which is how `.bss` costs no file space) |
| 24 | 4 | flags |

Flags: `0x01` EXEC, `0x02` WRITE, `0x04` READ, `0x10` METADATA (file only, never
mapped), `0x20` RELRO (writable while loading, read-only afterwards).
**EXEC and WRITE together are invalid** (W^X is part of the format).

Mapped sections must not overlap in memory; no section may overlap another or
the headers in the file. RVAs start at `section_align`, leaving the first page
unmapped as a guard.

## Metadata sections

All names are offsets into `.strtab` (NUL-terminated UTF-8, offset 0 = "").

| section | entry format | notes |
|---|---|---|
| `.edata` | `u32 name, u8 section, 3 pad, u32 offset, u32 size, u32 ordinal` (20 B) | **sorted strictly by name** (bytewise) so a loader may binary-search; `section`/`offset` locate the symbol, `size` 0 = unknown; ordinals unique and non-zero |
| `.impdir` | descriptor `u32 lib_name, u32 count` then `count` * `u32 name, u32 ordinal, u32 iat_rva, u32 flags` | library name stored once per library; flag `1` = import by ordinal (name ignored); each `iat_rva` is an 8-byte slot, 8-aligned, inside a **writable** section |
| `.reloc` | `u32 rva, u8 type, u8 target_section, u16 pad, i64 addend` (16 B) | see below |
| `.symtab` | `u32 name, u8 section, 3 pad, u32 offset` (12 B) | optional debug symbols |
| `.devtab` | `u16 vendor, u16 device, u32 class_code, u32 class_mask` (12 B) | **drivers only**: devices the driver supports; `0xFFFF` is a wildcard for vendor/device (a device with no vendor is invalid), class codes and masks are 24 bit. A device matches a row when vendor/device match and `(class ^ class_code) & class_mask == 0`; first match wins. Requires HAS_PROBE |

`.idata` is an ordinary mapped section (`RW`, flagged `RELRO`) holding
one 8-byte slot per import. The packer generates it; code reaches a slot with a
`REL32` relocation, so code never needs to know where `.idata` is.

### Relocations (RELA style: the addend is explicit)

`T = (rva of target_section, or 0 if target_section = 0xFF) + addend`

| type | meaning | value written |
|---|---|---|
| 1 `ABS64` | 8 bytes | `image_base + T` |
| 2 `ABS32` | 4 bytes | `image_base + T`; the loader maps such modules below 2 GiB (`MAP_32BIT`) and fails if it does not fit |
| 3 `REL32` | 4 bytes, signed | `T - (rva_of_patch + 4)`: base independent, for `call/lea/mov [rip+disp32]` whose disp32 is the *last* field of the instruction (otherwise subtract the trailing immediate size from the addend) |
| 4 `RVA32` | 4 bytes | `T` |

### Who may import what

| importer | may import |
|---|---|
| LXF, LLF, LKF | LLF |
| LDF | LLF, and at most **one** LKF (the kernel it binds to) |

Nothing may import an LXF or an LDF. A driver's `kabi_min..kabi_max` must
contain the kernel's `kabi`, otherwise the load fails. Kernel exports (sorted,
ordinals, forwarding rules as for any module) are its driver API.

## Loading procedure

1. Parse and validate everything (bounds, alignment, overlap, W^X, sorted
   exports, string offsets, IAT placement, entry points in EXEC sections).
2. Map an anonymous RW image (`preferred_base` as a hint), copy mapped sections
   to their RVAs, apply relocations. Recursively map imported libraries (a
   module is cached before recursing, so cycles are fine).
3. Bind imports by writing export addresses into IAT slots.
4. `mprotect` every section to its final permissions; everything else,
   including the guard page and gaps, becomes `PROT_NONE`. RELRO sections end up
   read-only.
5. Run `libmain(1)` in dependency order (a cycle is reported and broken at the
   back edge). A non-zero return aborts the load: modules already initialised
   get `libmain(0)` in reverse order and everything is unmapped.
6. On unload, `libmain(0)` runs in reverse initialisation order.

## ABI

SysV AMD64 calling convention.

* executable entry: `int entry(long argc, char **argv, char **envp)`; `eax` is the exit status (the runner exits with `eax & 0xFF`)
* kernel entry, `lordboot`: `int kernel_main(u64 boot_magic, struct bootinfo *bi)` with `boot_magic = 0x4C4F5244` ("LORD");
  `raw`: `int kernel_main(void)` with `rdi = rsi = 0`. `struct bootinfo { u32 magic; u32 version (=1); u64 cmdline; u64 mem_total; u32 ncpus; u32 reserved; }`, `cmdline` pointing at a NUL-terminated string
* driver hooks: `int probe(u32 vendor, u32 device, u32 class)` (0 = device claimed, non-zero = declined; offered only devices matching `.devtab`, or any device if the table is empty), `void remove(void)`; a `PERSISTENT` driver is never removed
* a kernel or driver `libmain` follows the library rules below; for a kernel imported by a driver it is the only code that runs (the kernel is not "booted")
* `int libmain(long reason)`: `1` load, `0` unload; return 0 for success
* exports: ordinary functions
* the format defines no syscall interface; the examples use Linux syscalls directly

The hosted runner (`run_lord.py`) cannot boot a real kernel: it calls the LKF
entry as an ordinary function in user space, and maps a driver's LKF only as an
API provider. The format itself is meant to be consumed by a real loader too.

Library names in imports must be bare file names; anything containing a path
separator, `..`, or an absolute path is rejected.

## Not implemented (deliberately deferred)

Export forwarders, lazy binding / PLT stubs, thread attach/detach reasons, TLS,
signatures (the CRC detects accidental corruption only, not tampering).
