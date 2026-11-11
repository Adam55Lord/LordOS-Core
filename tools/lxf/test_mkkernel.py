# SPDX-License-Identifier: GPL-2.0-only
# Copyright (C) 2026 Adam55Lord
"""Tests for mkkernel.py: python3 -m unittest -v   (run in tools/lxf; needs nothing else)."""
import os
import struct
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import lordfmt as L  # noqa: E402

BASE = 0xFFFFFFFF80000000


def make_elf(segs, entry, machine=62, etype=2):
    """Minimal static 64-bit ELF with PT_LOAD segments: (vaddr, data, memsz, p_flags)."""
    n = len(segs)
    hdr = bytearray(64)
    hdr[:4] = b"\x7fELF"
    hdr[4], hdr[5], hdr[6] = 2, 1, 1
    struct.pack_into("<HHIQQQIHHHHHH", hdr, 16, etype, machine, 1, entry, 64, 0, 0, 64, 56, n, 0, 0, 0)
    ph, body, off = b"", [], 64 + 56 * n
    for v, d, m, f in segs:
        off = (off + 4095) & ~4095
        ph += struct.pack("<IIQQQQQQ", 1, f, off, v, v, len(d), m, 4096)
        body.append((off, d))
    out = bytearray(hdr) + ph
    for o, d in body:
        out += b"\0" * (o - len(out)) + d
    return bytes(out)


def pack(elf, *extra):
    with tempfile.TemporaryDirectory() as t:
        i, o = os.path.join(t, "k.elf"), os.path.join(t, "k.lkf")
        with open(i, "wb") as f:
            f.write(elf)
        r = subprocess.run([sys.executable, os.path.join(HERE, "mkkernel.py"), i, o, *extra],
                           capture_output=True, text=True)
        data = None
        if r.returncode == 0:
            with open(o, "rb") as f:
                data = f.read()
        return r, data


CODE = b"\x90" * 100 + b"\xf4"
RO = b"ro" * 50
DATA = b"d" * 40


class MkKernel(unittest.TestCase):
    def test_contiguous_layout(self):
        elf = make_elf([(BASE, CODE, len(CODE), 5), (BASE + 0x1000, RO, 100, 4),
                        (BASE + 0x2000, DATA, 0x3000, 6)], BASE + 100)
        r, data = pack(elf, "--kernel-version", "1.2.3", "--kabi", "2", "--cmdline", "quiet", "--smp")
        self.assertEqual(r.returncode, 0, r.stderr)
        lf = L.LordFile.from_bytes(data)
        self.assertTrue(lf.is_kernel)
        k = lf.kernel
        self.assertEqual(k.load_addr, BASE - 0x1000)
        self.assertTrue(k.flags & L.KF_FIXED_ADDR and k.flags & L.KF_SMP and not k.flags & L.KF_DEBUG)
        self.assertEqual((k.kabi, k.cmdline, k.boot_proto), (2, "quiet", L.BOOT_LORD))
        self.assertEqual(k.version, L.pack_version("1.2.3"))
        self.assertEqual([(s.name, s.rva, s.mem_size) for s in lf.sections if s.mapped],
                         [(b".text", 0x1000, 101), (b".rodata", 0x2000, 100), (b".data", 0x3000, 0x3000)])
        self.assertEqual(lf.entry, (b".text", 100))
        self.assertEqual(lf.rva_of(*lf.entry) + k.load_addr, BASE + 100)   # entry virtual address

    def test_gaps_keep_linked_addresses(self):
        elf = make_elf([(BASE, CODE, len(CODE), 5), (BASE + 0x200000, RO, 100, 4)], BASE)
        r, data = pack(elf)
        self.assertEqual(r.returncode, 0, r.stderr)
        lf = L.LordFile.from_bytes(data)
        ro = lf.section(".rodata")
        self.assertEqual(ro.rva + lf.kernel.load_addr, BASE + 0x200000)
        gap = [s for s in lf.sections if s.name.startswith(b".gap")]
        self.assertEqual(len(gap), 1)
        self.assertEqual(gap[0].flags, 0)          # no access: the loader maps nothing there

    def test_debug_flag_and_build_id(self):
        elf = make_elf([(BASE, CODE, len(CODE), 5)], BASE)
        _, a = pack(elf, "--kdebug")
        _, b = pack(make_elf([(BASE, CODE + b"\x90", len(CODE) + 1, 5)], BASE), "--kdebug")
        ka, kb = L.LordFile.from_bytes(a).kernel, L.LordFile.from_bytes(b).kernel
        self.assertTrue(ka.flags & L.KF_DEBUG)
        self.assertNotEqual(ka.build_id, bytes(16))
        self.assertNotEqual(ka.build_id, kb.build_id)

    def test_refusals(self):
        for what, elf in (
            ("unaligned segment", make_elf([(BASE + 0x10, CODE, len(CODE), 5)], BASE + 0x10)),
            ("W^X segment", make_elf([(BASE, CODE, len(CODE), 7)], BASE)),
            ("entry outside code", make_elf([(BASE, CODE, len(CODE), 5), (BASE + 0x1000, RO, 100, 4)], BASE + 0x1000)),
            ("PIE", make_elf([(BASE, CODE, len(CODE), 5)], BASE, etype=3)),
            ("not an ELF", b"MZ" + bytes(100)),
        ):
            r, _ = pack(elf)
            self.assertNotEqual(r.returncode, 0, what)

    def test_other_arch_ids(self):
        r, data = pack(make_elf([(BASE, CODE, len(CODE), 5)], BASE, machine=183))
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(struct.unpack_from("<H", data, 6)[0], 2)           # AArch64 skeleton id


if __name__ == "__main__":
    unittest.main()
