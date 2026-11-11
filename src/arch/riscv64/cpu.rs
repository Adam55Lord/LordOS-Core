// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Adam55Lord

//! Halting, interrupt flag, spin hint.

use core::arch::asm;

pub fn halt() {
    // SAFETY: `wfi` only waits for an interrupt.
    unsafe { asm!("wfi", options(nomem, nostack, preserves_flags)) };
}

/// Unmasks interrupts and waits for one (`wfi` wakes on a pending interrupt even if masked).
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
    // SAFETY: clearing SIE is always sound in S-mode.
    unsafe { asm!("csrci sstatus, 2", options(nomem, nostack)) };
}

pub fn enable_interrupts() {
    // SAFETY: only called where the trap vector is installed.
    unsafe { asm!("csrsi sstatus, 2", options(nomem, nostack)) };
}

pub fn pause() {
    core::hint::spin_loop();
}

/// Raises a breakpoint exception.
pub fn breakpoint() {
    // SAFETY: `ebreak` raises a synchronous exception the trap vector handles.
    unsafe { asm!("ebreak", options(nomem, nostack)) };
}
