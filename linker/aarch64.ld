/* SPDX-License-Identifier: GPL-2.0-only
 * Copyright (C) 2026 Adam55Lord */

/* AArch64 kernel, higher half. SKELETON: written for the port, not yet booted. Check the base
 * address against the MMU layout the port chooses (src/arch/aarch64/paging.rs). Same sections and
 * segment split as linker/x86_64.ld, so tools/lxf/mkkernel.py packs it unchanged. */
OUTPUT_FORMAT(elf64-littleaarch64)
ENTRY(_start)

PHDRS
{
    text   PT_LOAD;
    rodata PT_LOAD;
    data   PT_LOAD;
}

SECTIONS
{
    . = 0xffffffff80000000;

    .text : {
        *(.text .text.*)
    } :text

    . = ALIGN(CONSTANT(MAXPAGESIZE));

    .rodata : {
        *(.rodata .rodata.*)
    } :rodata

    . = ALIGN(CONSTANT(MAXPAGESIZE));

    .data : {
        *(.data .data.*)
    } :data

    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
        *(.comment)
    }
}
