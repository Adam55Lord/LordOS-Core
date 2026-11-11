// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Adam55Lord

//! Placeholder boot backend for the skeleton ports (aarch64, riscv64).
//!
//! It exists so the kernel links on those architectures. It assumes the loader entered the
//! kernel in the higher half with a usable stack and passes an **empty** [`BootInfo`] to `kmain`,
//! which then stops at the first architecture stub (`todo!()`) with a message naming it.
//!
//! A real backend replaces this file: parse the loader's hand-off (UEFI memory map + ACPI or a
//! device tree, a framebuffer if any, the CPU list), fill `BootInfo` and call `kmain`. The
//! checklist is in `docs/20-bootloader-porting.md`.

use super::BootInfo;

/// Kernel entry point (`ENTRY(_start)` in `linker/<arch>.ld`).
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    let boot = BootInfo {
        bootloader: None,
        firmware: None,
        hhdm_offset: None,
        memory_map: &[],
        memory_map_dropped: 0,
        framebuffer: None,
        rsdp_phys: None,
        smp: None,
    };
    crate::kmain(&boot)
}
