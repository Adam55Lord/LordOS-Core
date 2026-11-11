// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Adam55Lord

//! Local interrupt-enable state (RFLAGS.IF). Used by the interrupt-masking locks in `hal::sync`.

use core::arch::asm;

/// Is the interrupt flag set on this CPU?
pub fn enabled() -> bool {
    let flags: u64;
    // SAFETY: pushfq/pop only reads RFLAGS.
    unsafe { asm!("pushfq", "pop {}", out(reg) flags, options(preserves_flags)) };
    flags & (1 << 9) != 0
}

/// Masks interrupts on this CPU and returns whether they were enabled (pass it to [`restore`]).
pub fn save() -> bool {
    let was = enabled();
    // SAFETY: clearing IF is always sound in ring 0.
    unsafe { asm!("cli", options(nomem, nostack)) };
    was
}

/// Re-enables interrupts if `was_enabled`.
pub fn restore(was_enabled: bool) {
    if was_enabled {
        // SAFETY: interrupts were enabled when `save` ran.
        unsafe { asm!("sti", options(nomem, nostack)) };
    }
}

/// What a CPU does while it spins on a lock or flag: answer a pending TLB-shootdown request.
/// A CPU that waits with interrupts masked cannot take the IPI, so without this a shootdown
/// initiator holding a lock could wait forever for a CPU that waits for that lock.
#[inline(always)]
pub fn spin_hook() {
    super::tlb::poll();
}
