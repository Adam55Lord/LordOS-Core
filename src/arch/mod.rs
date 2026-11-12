// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Adam55Lord

//! Architecture layer: everything that depends on the CPU architecture lives below this module.
//!
//! The rest of the kernel (`mm`, `ob`, `ps`, `ipc`, `ex`, `ld`, `hal::{memory, sync, display}`,
//! `main.rs`) is architecture-neutral and talks to the machine **only** through the names
//! re-exported here. Exactly one backend is compiled, chosen by `target_arch`:
//!
//! | `target_arch` | backend            | state                                   |
//! |---------------|--------------------|-----------------------------------------|
//! | `x86_64`      | [`x86_64`]         | complete, boots under QEMU/OVMF         |
//! | `aarch64`     | `aarch64`          | skeleton: contract stubs, not bootable  |
//! | `riscv64`     | `riscv64`          | skeleton: contract stubs, not bootable  |
//!
//! # The backend contract
//!
//! A backend is a module with these public items (the x86_64 backend is the reference
//! implementation; `docs/23-architecture-abstraction.md` explains each one):
//!
//! | item                  | what the generic kernel uses it for                              |
//! |-----------------------|------------------------------------------------------------------|
//! | `NAME`                | architecture name for logs (`"x86_64"`)                          |
//! | `LXF_MACHINE`         | machine id an LXF image must carry to be loadable here           |
//! | `PAGE_SIZE`           | smallest page (4 KiB on all three)                               |
//! | `cpu`                 | halt, interrupt flag, pause, breakpoint, CPU identity/topology   |
//! | `irq`                 | save / restore / query the local interrupt-enable state          |
//! | `percpu`              | per-CPU data pointer (GS base / `tpidr_el1` / `tp`)              |
//! | `paging`              | MMU: map, unmap, protect, translate, roots, neutral `pte` flags  |
//! | `tlb`                 | cross-CPU TLB shoot-down                                         |
//! | `smp`                 | start / stop secondary CPUs                                      |
//! | `context`             | kernel context switch, initial thread stack, per-CPU kernel stack |
//! | `usermode`            | drop to user mode; built-in user test programs                   |
//! | `trap`                | `TrapFrame`, syscall entry, user-fault hook, fault address       |
//! | `ipi`                 | reschedule inter-processor interrupts                            |
//! | `tick`                | periodic scheduler tick                                          |
//! | `serial`              | debug UART (`COM1` lock, `init`)                                 |
//! | `bringup`             | the ordered platform bring-up steps `kmain` calls                |
//!
//! Anything an architecture needs beyond that (GDT/IDT/APIC on x86, GIC/PSCI on arm64,
//! PLIC/SBI on RISC-V) stays private to its backend.

// `pub(crate)`: the x86-only Lord Boot backend (src/boot/lord_boot) needs x86 internals (APIC, PIT).
#[cfg(target_arch = "x86_64")]
pub(crate) mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use self::x86_64::*;

// The skeleton ports start from the shared contract stubs and replace them module by module.
#[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
mod skeleton;

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "aarch64")]
pub use self::aarch64::*;

#[cfg(target_arch = "riscv64")]
mod riscv64;
#[cfg(target_arch = "riscv64")]
pub use self::riscv64::*;

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64")))]
compile_error!(
    "Lordixyn Core has no backend for this target_arch. Add src/arch/<name>/ and list it in \
     src/arch/mod.rs (see docs/23-architecture-abstraction.md)."
);
