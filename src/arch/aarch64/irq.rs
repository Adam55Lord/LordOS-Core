// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Adam55Lord

//! Local interrupt masking: the I bit of `DAIF` (IRQ mask).

use core::arch::asm;

const DAIF_I: u64 = 1 << 7;

/// Are IRQs unmasked on this CPU?
pub fn enabled() -> bool {
    let daif: u64;
    // SAFETY: reading DAIF has no side effects.
    unsafe { asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack, preserves_flags)) };
    daif & DAIF_I == 0
}

/// Masks IRQs and returns whether they were enabled (pass it to [`restore`]).
pub fn save() -> bool {
    let was = enabled();
    // SAFETY: masking IRQs is always sound at EL1.
    unsafe { asm!("msr daifset, #2", options(nomem, nostack)) };
    was
}

/// Unmasks IRQs if `was_enabled`.
pub fn restore(was_enabled: bool) {
    if was_enabled {
        // SAFETY: IRQs were enabled when `save` ran.
        unsafe { asm!("msr daifclr, #2", options(nomem, nostack)) };
    }
}

/// What a CPU does while it spins on a lock. Port: answer a pending TLB-shootdown request
/// here once `tlb::poll` exists.
#[inline(always)]
pub fn spin_hook() {
    super::tlb::poll();
}
