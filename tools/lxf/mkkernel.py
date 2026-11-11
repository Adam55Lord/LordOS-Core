#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
# Copyright (C) 2026 Adam55Lord
"""
mkkernel.py -- pack the freshly linked Lord Core kernel into an LKF (Lord Kernel Format) image.

    python3 mkkernel.py target/x86_64-unknown-none/release/lord-core dist/lord-core.lkf \
            [--kernel-version 0.1.0] [--kabi 1] [--cmdline "..."] [--smp] [--kdebug]

This is the last step of lord-core's ./build.sh, not a general converter: the Rust linker can
only write its result in its own executable format, and this tool reads exactly that one file
(static, non-PIE, little endian) and nothing else. Its output (LKF, format version 2, see
SPEC.md) is the only kernel format Lord Boot loads; the linker output is never shipped.

flat2lord.py wraps a *flat* binary that can run at any address. A kernel cannot: it is linked
for a fixed higher-half address. The LKF way to say that is

  * one mapped section per linked PT_LOAD segment (.text r-x, .rodata r--, .data rw-; the
    segment's .bss is the section's virtual_size, zero-filled by the loader),
  * header extension: FIXED_ADDR + load_addr, so the loader must map the image exactly where
    the kernel was linked,
  * entry_rva = the entry point's RVA, boot protocol "lordboot".

RVAs in LKF start at section_align (the first page is an unmapped guard), so
load_addr = <lowest segment vaddr> - 0x1000 and every section's RVA is vaddr - load_addr.
Gaps between linked segments (if the linker leaves any) become no-access filler sections so
the RVAs - and therefore every absolute address inside the kernel - stay exactly as linked.

Only what a loader needs is kept: no ELF headers, no section table, no symbols.
Needs nothing but Python 3 (the linker output is parsed here, no objcopy / pyelftools).
"""
import argparse
import hashlib
import os
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from lordfmt import (  # noqa: E402
    LordFile, LordFormatError, KernelInfo, Section,
    TYPE_KERNEL, ARCH_X86_64, SEC_EXEC, SEC_WRITE, SEC_READ,
    BOOT_LORD, KF_FIXED_ADDR, KF_SMP, KF_DEBUG, pack_version, align_up,
)

ET_EXEC = 2
# ELF e_machine -> arch id of the header. The format defines 1 = x86-64; the skeleton ports use
# the ids the kernel's `arch::LXF_MACHINE` has always had (2 = AArch64, 3 = RISC-V 64).
EM_X86_64, EM_AARCH64, EM_RISCV = 62, 183, 243
MACHINES = {EM_X86_64: ARCH_X86_64, EM_AARCH64: 2, EM_RISCV: 3}
PT_LOAD = 1
PF_X, PF_W, PF_R = 1, 2, 4
PAGE = 0x1000
GUARD = PAGE            # LKF reserves RVA 0 .. section_align as an unmapped guard


def read_linked(path):
    """Return (entry_vaddr, [(vaddr, filebytes, memsz, sec_flags), ...], arch_id)."""
    with open(path, "rb") as f:
        img = f.read()
    if img[:4] != b"\x7fELF":
        sys.exit(f"{path}: not a statically linked kernel (linker output expected)")
    if img[4] != 2 or img[5] != 1:
        sys.exit(f"{path}: need a 64-bit little-endian kernel")
    (e_type, e_machine, _ver, e_entry, e_phoff, _shoff, _flags, _ehsize,
     e_phentsize, e_phnum) = struct.unpack_from("<HHIQQQIHHH", img, 16)
    if e_machine not in MACHINES:
        sys.exit(f"{path}: unsupported ELF machine {e_machine} (supported: x86-64, AArch64, RISC-V)")
    if e_type != ET_EXEC:
        sys.exit(f"{path}: not a static executable (type {e_type}). A kernel linked as a PIE cannot be "
                 "packed; check .cargo/config.toml (relocation-model=static) and build.rs (--no-pie).")
    segs = []
    for i in range(e_phnum):
        (p_type, p_flags, p_offset, p_vaddr, _paddr, p_filesz, p_memsz, _align) = \
            struct.unpack_from("<IIQQQQQQ", img, e_phoff + i * e_phentsize)
        if p_type != PT_LOAD or p_memsz == 0:
            continue
        if p_offset + p_filesz > len(img):
            sys.exit(f"{path}: segment {i} runs past the end of the file")
        flags = SEC_READ                                   # every segment is readable
        if p_flags & PF_W:
            flags |= SEC_WRITE
        if p_flags & PF_X:
            flags |= SEC_EXEC
        segs.append((p_vaddr, img[p_offset:p_offset + p_filesz], p_memsz, flags))
    if not segs:
        sys.exit(f"{path}: no PT_LOAD segments")
    return e_entry, segs, MACHINES[e_machine]


def section_name(flags, used):
    base = b".text" if flags & SEC_EXEC else b".data" if flags & SEC_WRITE else b".rodata"
    name, n = base, 1
    while name in used:
        n += 1
        name = base[:7 - len(str(n))] + str(n).encode()
    used.add(name)
    return name


def build(entry_vaddr, segs, arch, version, kabi, cmdline, smp, kdebug):
    """Lay the segments out at their linked addresses and build the LordFile."""
    segs = sorted(segs, key=lambda s: s[0])
    for vaddr, data, memsz, flags in segs:
        if vaddr % PAGE:
            sys.exit(f"segment at {vaddr:#x} is not 4 KiB aligned; link with page-aligned segments "
                     "(see linker/<arch>.ld: ALIGN(CONSTANT(MAXPAGESIZE)))")
        if flags & SEC_WRITE and flags & SEC_EXEC:
            sys.exit(f"segment at {vaddr:#x} is writable and executable (W^X)")
        if len(data) > memsz:
            sys.exit(f"segment at {vaddr:#x}: filesz > memsz")
    load_addr = segs[0][0] - GUARD
    if load_addr < 0:
        sys.exit("kernel linked too low: the image needs one guard page below its first segment")

    lf = LordFile(TYPE_KERNEL, arch)
    lf.file_align = PAGE                                  # sections mmap-able / page copyable
    lf.kernel = KernelInfo(version=version, kabi=kabi, boot_proto=BOOT_LORD,
                           flags=KF_FIXED_ADDR | (KF_SMP if smp else 0) | (KF_DEBUG if kdebug else 0),
                           cmdline=cmdline, load_addr=load_addr)
    used, cursor, entry = set(), GUARD, None              # cursor = RVA the next section gets
    for vaddr, data, memsz, flags in segs:
        rva = vaddr - load_addr
        if rva < cursor:
            sys.exit(f"segment at {vaddr:#x} overlaps the previous one")
        if rva > cursor:                                  # keep the linked gap
            gap = b".gap%d" % len([n for n in used if n.startswith(b".gap")])
            used.add(gap)
            lf.add_section(gap, b"", 0, rva - cursor)
            cursor = rva
        name = section_name(flags, used)
        lf.add_section(name, data, flags, memsz)
        if flags & SEC_EXEC and vaddr <= entry_vaddr < vaddr + len(data):
            entry = (name, entry_vaddr - vaddr)
        cursor = align_up(rva + memsz, PAGE)
    if entry is None:
        sys.exit(f"entry point {entry_vaddr:#x} is not inside an executable segment")
    lf.entry = entry
    # Same default as flat2lord.py: the first 16 bytes of SHA-256 over the executable code.
    code = b"".join(s.data for s in lf.sections if s.flags & SEC_EXEC)
    lf.kernel.build_id = hashlib.sha256(code).digest()[:16]
    return lf, load_addr


def main():
    ap = argparse.ArgumentParser(description="Pack the linked Lord Core kernel into an LKF image (.lkf)")
    ap.add_argument("input", help="the statically linked kernel (Cargo's output in target/)")
    ap.add_argument("output", help="output .lkf file")
    ap.add_argument("--kernel-version", default="0.0.0", help="X[.Y[.Z]] (default 0.0.0)")
    ap.add_argument("--kabi", type=int, default=1, help="kernel ABI level drivers are checked against")
    ap.add_argument("--cmdline", default="", help="default kernel command line")
    ap.add_argument("--smp", action="store_true", help="set the SMP flag")
    ap.add_argument("--kdebug", action="store_true", help="set the DEBUG flag")
    args = ap.parse_args()

    entry, segs, arch = read_linked(args.input)
    try:
        lf, load_addr = build(entry, segs, arch, pack_version(args.kernel_version), args.kabi,
                              args.cmdline, args.smp, args.kdebug)
        blob = lf.to_bytes()
        back = LordFile.from_bytes(blob)                  # prove it round-trips through the strict parser
    except LordFormatError as e:
        sys.exit(f"invalid kernel layout: {e}")
    assert back.is_kernel and back.kernel.load_addr == load_addr
    assert [(s.name, s.rva, s.mem_size) for s in back.sections if s.mapped] == \
           [(s.name, s.rva, s.mem_size) for s in lf.sections if s.mapped]
    for s in back.sections:                               # RVA + load_addr must be the linked address
        if s.mapped and s.name.startswith((b".text", b".rodata", b".data")):
            assert s.rva + load_addr in {v for v, *_ in segs}, s.name

    with open(args.output, "wb") as f:
        f.write(blob)
    print(f"wrote {args.output} ({len(blob)} bytes) from {args.input}")
    print(f"  entry {entry:#x}  load_addr {load_addr:#x} (fixed)")
    print(back.describe())


if __name__ == "__main__":
    main()
