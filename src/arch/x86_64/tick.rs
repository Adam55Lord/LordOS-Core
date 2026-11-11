// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Adam55Lord

//! The periodic scheduler tick of the calling CPU (the LAPIC timer here).

use super::apic_timer;
use super::trap::TrapHandler;

/// Why a tick could not be started.
pub use super::apic_timer::TimerError as TickError;

/// Installs the function every CPU runs on each tick (shared by all CPUs).
pub fn set_hook(hook: TrapHandler) {
    apic_timer::set_tick_hook(hook);
}

/// Starts the periodic tick on the calling CPU at `hz`.
pub fn start_periodic_hz(hz: u32) -> Result<(), TickError> {
    apic_timer::start_periodic_hz(hz)
}
