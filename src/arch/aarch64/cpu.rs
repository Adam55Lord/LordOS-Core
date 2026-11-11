// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Adam55Lord

//! Halting, interrupt flag, spin hint.

use core::arch::asm;

pub fn halt() {
    // SAFETY: `wfi` only waits for an interrupt.
    unsafe { asm!("wfi", options(nomem, nostack, preserves_flags)) };
}

/// Unmasks IRQs and waits for one, without a window in between (`wfi` wakes even when masked,
/// so unmasking first is enough here).
pub fn enable_and_halt() {
    enable_interrupts();
    halt();
}

pub fn halt_forever() -> ! {
    disable_interrupts();
    loop {
        halt();
    }
}

pub fn disable_interrupts() {
    // SAFETY: masking IRQs is always sound at EL1.
    unsafe { asm!("msr daifset, #2", options(nomem, nostack)) };
}

pub fn enable_interrupts() {
    // SAFETY: only called where the exception vectors are installed.
    unsafe { asm!("msr daifclr, #2", options(nomem, nostack)) };
}

pub fn pause() {
    core::hint::spin_loop();
}

/// Raises a breakpoint exception.
pub fn breakpoint() {
    // SAFETY: `brk` raises a synchronous exception the vectors handle.
    unsafe { asm!("brk #0", options(nomem, nostack)) };
}
