// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Adam55Lord

//! x86_64 backend: the reference implementation of the architecture contract
//! (see `src/arch/mod.rs`). Everything here may use `asm!`, ports, MSRs, GDT/IDT/TSS, the
//! 8259 PIC, the Local APIC, the HPET/PIT and 4-level paging; nothing outside `src/arch/`
//! may name them.

#![allow(dead_code)] // a full hardware layer: not every register helper is used yet

/// Architecture name, as printed in the boot log.
pub const NAME: &str = "x86_64";

/// The machine id an LXF image must carry to be loadable here (`ARCH_X86_64` in the format).
pub const LXF_MACHINE: u16 = 1;

/// The smallest page the MMU maps.
pub const PAGE_SIZE: u64 = 4096;

// Contract modules: the generic kernel may use these names.
pub mod bringup;
pub mod context;
pub mod cpu;
pub mod ipi;
pub mod irq;
pub mod paging;
pub mod percpu;
pub mod serial;
pub mod smp;
pub mod tick;
pub mod tlb;
pub mod trap;
pub mod usermode;

// x86 internals: reachable from this backend (and the Lord Boot AP trampoline) only.
pub(crate) mod apic;
pub(crate) mod apic_timer;
pub(crate) mod gdt;
pub(crate) mod hpet;
pub(crate) mod idt;
pub(crate) mod msr;
pub(crate) mod pic;
pub(crate) mod pit;
pub(crate) mod port;
pub mod syscall;
pub(crate) mod timer;
pub(crate) mod tss;
pub(crate) mod uart16550;
