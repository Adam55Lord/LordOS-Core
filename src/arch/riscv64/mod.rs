// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Adam55Lord

//! RISC-V (RV64GC, S-mode) backend: **skeleton**. It type-checks, links and has working
//! interrupt masking and CPU halt, but nothing else: every other contract item is a `todo!()`
//! stub from `arch/skeleton.rs`. See `docs/23-architecture-abstraction.md`, section "Porting
//! to riscv64", for the order to fill it in (trap vector, Sv48 MMU, PLIC/AIA, SBI timer + HSM, ...).

#![allow(dead_code)]

pub use super::skeleton::*;

/// Architecture name, as printed in the boot log.
pub const NAME: &str = "riscv64";

/// The machine id an LXF image must carry (`ARCH_RISCV64` in the format).
pub const LXF_MACHINE: u16 = 3;

/// The smallest page the MMU maps.
pub const PAGE_SIZE: u64 = 4096;

pub mod cpu;
pub mod irq;
