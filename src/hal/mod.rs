// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Adam55Lord

//! Hardware Abstraction Layer (HAL), first cut.
//!
//! Everything that touches the machine directly lives under here so the rest
//! of the kernel never uses ports, `asm!` or raw framebuffer pointers:
//!
//! | module      | what it abstracts                                   |
//! |-------------|-----------------------------------------------------|
//! | `display`   | framebuffer text console (debug channel on screen)  |
//! | `font`      | the console's bitmap font                           |
//! | `memory`    | physical memory map and the HHDM                    |
//! | `sync`      | spin locks, interrupt-masking locks                 |
//! | `acpi`      | ACPI table discovery (RSDP -> XSDT/RSDT -> find by signature) |
//!
//! Everything that depends on the CPU architecture (ports, GDT/IDT, APIC, MMU, SMP, context
//! switching, ...) lives in [`crate::arch`], not here.
#![allow(dead_code)]

/// Maximum number of logical CPUs (hardware threads) the kernel supports.
/// Each one costs ~100 KiB of statically allocated stacks.
pub const MAX_CPUS: usize = 16;

pub mod acpi;
pub mod display;
pub mod font;
pub mod memory;
pub mod sync;

/// What came up during [`init`].
pub struct HalStatus {
    pub serial: bool,
    pub display: Option<display::Geometry>,
}

/// Brings up the output devices. Call this first: after it returns, the
/// logging macros reach whichever devices are available.
pub fn init(framebuffer: Option<display::FramebufferInfo>) -> HalStatus {
    let serial = crate::arch::serial::init();
    let display = framebuffer.and_then(display::init);
    HalStatus { serial, display }
}

/// A device the platform provides, to be published in the Object Manager namespace.
pub struct PlatformDevice {
    pub path: &'static str,
    pub class: &'static str,
    pub description: &'static str,
    pub base: u64,
    pub size: u64,
    /// `base` is an I/O port rather than a physical address.
    pub io_port: bool,
}
