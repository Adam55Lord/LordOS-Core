// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Adam55Lord

//! Local interrupt masking: `sstatus.SIE`.

use core::arch::asm;

const SSTATUS_SIE: usize = 1 << 1;

/// Are supervisor interrupts enabled on this hart?
pub fn enabled() -> bool {
    let sstatus: usize;
    // SAFETY: reading sstatus has no side effects.
    unsafe { asm!("csrr {}, sstatus", out(reg) sstatus, options(nomem, nostack, preserves_flags)) };
    sstatus & SSTATUS_SIE != 0
}

/// Masks interrupts and returns whether they were enabled (pass it to [`restore`]).
pub fn save() -> bool {
    let sstatus: usize;
    // SAFETY: clearing SIE is always sound in S-mode; the old value comes back atomically.
    unsafe { asm!("csrrci {}, sstatus, 2", out(reg) sstatus, options(nomem, nostack)) };
    sstatus & SSTATUS_SIE != 0
}

/// Re-enables interrupts if `was_enabled`.
pub fn restore(was_enabled: bool) {
    if was_enabled {
        // SAFETY: interrupts were enabled when `save` ran.
        unsafe { asm!("csrsi sstatus, 2", options(nomem, nostack)) };
    }
}

/// What a hart does while it spins on a lock (answer a pending TLB-shootdown request).
#[inline(always)]
pub fn spin_hook() {
    super::tlb::poll();
}
