// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Adam55Lord

//! AArch64 backend: **skeleton**. It type-checks, links and has working interrupt masking and
//! CPU halt, but nothing else: every other contract item is a `todo!()` stub from
//! `arch/skeleton.rs`. See `docs/23-architecture-abstraction.md`, section "Porting to aarch64",
//! for the order to fill it in (exception vectors, MMU, GIC, generic timer, PSCI, ...).

#![allow(dead_code)]

pub use super::skeleton::*;

/// Architecture name, as printed in the boot log.
pub const NAME: &str = "aarch64";

/// The machine id an LXF image must carry (`ARCH_AARCH64` in the format).
pub const LXF_MACHINE: u16 = 2;

/// The smallest page the MMU maps (4 KiB granule).
pub const PAGE_SIZE: u64 = 4096;

pub mod cpu;
pub mod irq;
