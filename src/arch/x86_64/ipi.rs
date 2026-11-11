// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Adam55Lord

//! Reschedule inter-processor interrupts (the scheduler's only IPI; TLB shoot-down has its own
//! in `tlb.rs`). Backed by the Local APIC here.

use super::apic;
use super::trap::TrapHandler;

/// Asks `cpu` (a logical CPU index) to run its scheduler.
pub fn send_resched(cpu: usize) {
    apic::send_ipi_to_cpu(cpu, apic::RESCHED_VECTOR);
}

/// Asks every other online CPU to run its scheduler.
pub fn send_resched_all_but_self() {
    apic::send_ipi_all_but_self(apic::RESCHED_VECTOR);
}

/// Installs the function the receiving CPU runs for a reschedule IPI.
pub fn set_resched_hook(hook: TrapHandler) {
    apic::set_resched_hook(hook);
}

/// How many reschedule IPIs `cpu` has received.
pub fn resched_received(cpu: usize) -> u64 {
    apic::resched_ipis_of(cpu)
}
