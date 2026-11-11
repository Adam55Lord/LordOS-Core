// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Adam55Lord

//! TLB shootdown: making every CPU forget a translation that was just changed.
//!
//! `invlpg` only drops entries of the CPU that executes it. Once several CPUs run kernel code
//! (and threads of one process run on several CPUs), a page that is unmapped or made read-only
//! must be flushed on *every* CPU that may have cached it **before** the frame is reused or the
//! caller relies on the new protection.
//!
//! # Who gets the IPI
//!
//! * a **kernel-half** address (>= `0xffff_8000_0000_0000`): every online CPU, because the kernel
//!   half is shared by all address spaces;
//! * a **user** address: only the CPUs whose CR3 currently holds that address space's root
//!   ([`note_root`] records it on every `load_root`). The CPU publishes the new root with a
//!   sequentially consistent store *before* it writes CR3, and the initiator issues a full fence
//!   *after* editing the page table and *before* reading the roots, so one of the two always
//!   sees the other: either the initiator sends the IPI, or the loading CPU walks the new tables.
//!
//! # Protocol
//!
//! One request is in flight at a time (a spin lock serialises initiators): the initiator
//! writes the range into [`REQ_ADDR`] / [`REQ_PAGES`], sets one bit per target CPU in
//! [`PENDING`], sends [`TLB_VECTOR`] and waits until the mask is empty. A target flushes and
//! clears its own bit.
//!
//! # No deadlock with interrupts masked
//!
//! A CPU spinning on a lock with interrupts off cannot take the IPI. So every spin loop in the
//! kernel (`SpinLock::lock`, the waits in the scheduler, this module's own wait) calls [`poll`],
//! which answers a pending request *without* an interrupt. The flush itself takes no lock, so
//! it is safe anywhere.

use core::arch::asm;
use core::sync::atomic::{fence, AtomicU64, Ordering};

use crate::hal::sync::SpinLock;
use super::{apic, cpu, paging, percpu};
use crate::hal::MAX_CPUS;

/// Vector of the TLB-shootdown IPI.
pub const TLB_VECTOR: u8 = 0xF1;

/// First address of the kernel half (canonical upper half).
const KERNEL_HALF: u64 = 0xffff_8000_0000_0000;
/// Above this many pages a full flush (CR3 reload) is cheaper than page-by-page `invlpg`.
const FULL_FLUSH_PAGES: u64 = 32;

/// A CPU that does not answer within this many TSC ticks (several seconds) is considered hung.
const WAIT_LIMIT_TSC: u64 = 20_000_000_000;

/// CPUs that can hold translations (set by `percpu::init`).
static ONLINE: AtomicU64 = AtomicU64::new(0);
/// CPUs that still have to flush for the request in flight.
static PENDING: AtomicU64 = AtomicU64::new(0);
static REQ_ADDR: AtomicU64 = AtomicU64::new(0);
/// 0 = flush everything.
static REQ_PAGES: AtomicU64 = AtomicU64::new(0);
static INITIATORS: SpinLock<()> = SpinLock::new(());

const NO_ROOT: AtomicU64 = AtomicU64::new(0);
/// The PML4 each CPU has loaded (0 = unknown: always flushed).
static LOADED: [AtomicU64; MAX_CPUS] = [NO_ROOT; MAX_CPUS];

static SHOOTDOWNS: AtomicU64 = AtomicU64::new(0);
static IPIS: AtomicU64 = AtomicU64::new(0);
static LOCAL_ONLY: AtomicU64 = AtomicU64::new(0);
const NO_COUNT: AtomicU64 = AtomicU64::new(0);
static SERVED: [AtomicU64; MAX_CPUS] = [NO_COUNT; MAX_CPUS];

/// Counters for the boot log and the self-tests.
#[derive(Clone, Copy, Debug)]
pub struct TlbStats {
    /// Shootdown requests that needed at least one remote CPU.
    pub shootdowns: u64,
    /// Requests satisfied by the local flush alone (single CPU, or nobody else had it cached).
    pub local_only: u64,
    /// IPIs sent.
    pub ipis: u64,
}

pub fn stats() -> TlbStats {
    TlbStats {
        shootdowns: SHOOTDOWNS.load(Ordering::Relaxed),
        local_only: LOCAL_ONLY.load(Ordering::Relaxed),
        ipis: IPIS.load(Ordering::Relaxed),
    }
}

/// Remote flushes performed by CPU `cpu` so far.
pub fn served_by(cpu: usize) -> u64 {
    SERVED.get(cpu).map_or(0, |c| c.load(Ordering::Relaxed))
}

/// Marks `index` as a CPU that may cache translations, and flushes it, so everything changed
/// before this point is irrelevant and everything changed afterwards reaches it by IPI.
/// Called by `percpu::init` on the CPU itself.
pub fn cpu_online(index: usize) {
    LOADED[index].store(paging::current_root(), Ordering::SeqCst);
    ONLINE.fetch_or(1 << index, Ordering::SeqCst);
    flush_local(0, 0);
}

/// Records that this CPU is about to load `root` into CR3 (see the module docs for the ordering).
pub fn note_root(root: u64) {
    if let Some(i) = percpu::try_current_index() {
        LOADED[i].swap(root, Ordering::SeqCst);
    }
}

/// Is `root` loaded on a CPU other than the calling one? (The owner of an address space must not
/// free it while a CPU still walks its page tables.)
pub fn root_loaded_elsewhere(root: u64) -> bool {
    let me = percpu::try_current_index();
    let online = ONLINE.load(Ordering::Acquire);
    (0..MAX_CPUS).any(|c| Some(c) != me && online >> c & 1 != 0 && LOADED[c].load(Ordering::SeqCst) == root)
}

/// Flushes `pages` pages from `addr` (or everything when `pages == 0` or large) on THIS CPU.
fn flush_local(addr: u64, pages: u64) {
    if pages == 0 || pages > FULL_FLUSH_PAGES {
        let cr3 = paging::cr3();
        // SAFETY: reloading CR3 with its own value flushes the non-global TLB entries.
        unsafe { asm!("mov cr3, {}", in(reg) cr3, options(nostack, preserves_flags)) };
    } else {
        for i in 0..pages {
            let a = addr + i * 4096;
            // SAFETY: INVLPG only drops cached translations.
            unsafe { asm!("invlpg [{}]", in(reg) a, options(nostack, preserves_flags)) };
        }
    }
}

/// Answers the request in flight if it names this CPU. Takes no lock; callable from anywhere.
fn service(me: usize) {
    let bit = 1u64 << me;
    if PENDING.load(Ordering::Acquire) & bit == 0 {
        return;
    }
    flush_local(REQ_ADDR.load(Ordering::Relaxed), REQ_PAGES.load(Ordering::Relaxed));
    SERVED[me].fetch_add(1, Ordering::Relaxed);
    PENDING.fetch_and(!bit, Ordering::Release);
}

/// Spin-loop hook: serves a pending shootdown without needing an interrupt.
#[inline]
pub fn poll() {
    if PENDING.load(Ordering::Relaxed) != 0 {
        if let Some(me) = percpu::try_current_index() {
            service(me);
        }
    }
}

/// The IPI handler.
pub fn handler(_frame: &mut super::idt::InterruptFrame) {
    apic::eoi();
    if let Some(me) = percpu::try_current_index() {
        service(me);
    }
}

/// Makes the change of `pages` pages at `addr` (in the address space whose PML4 is `root`; the
/// root is ignored for kernel addresses) visible to every CPU: flushes here and interrupts the
/// others that may have it cached. Returns when all of them have flushed.
///
/// `pages == 0` flushes everything. Safe to call with interrupts masked and with kernel locks held.
pub fn shootdown(root: u64, addr: u64, pages: u64) {
    flush_local(addr, pages);
    let online = ONLINE.load(Ordering::Acquire);
    if online & online.wrapping_sub(1) == 0 {
        return; // zero or one CPU online: nobody to tell
    }
    let Some(me) = percpu::try_current_index() else {
        return;
    };
    let was = crate::hal::sync::irq_save();
    let _g = INITIATORS.lock(); // spins with `poll()`; its atomic exchange is the full fence
    fence(Ordering::SeqCst);
    let kernel = pages == 0 || addr >= KERNEL_HALF;
    let mut targets = 0u64;
    for c in 0..MAX_CPUS {
        if c == me || online >> c & 1 == 0 {
            continue;
        }
        let r = LOADED[c].load(Ordering::SeqCst);
        if kernel || r == 0 || r == root {
            targets |= 1 << c;
        }
    }
    if targets == 0 {
        LOCAL_ONLY.fetch_add(1, Ordering::Relaxed);
        drop(_g);
        crate::hal::sync::irq_restore(was);
        return;
    }
    SHOOTDOWNS.fetch_add(1, Ordering::Relaxed);
    REQ_ADDR.store(addr, Ordering::Relaxed);
    REQ_PAGES.store(pages, Ordering::Relaxed);
    PENDING.store(targets, Ordering::Release);
    for c in 0..MAX_CPUS {
        if targets >> c & 1 != 0 {
            IPIS.fetch_add(1, Ordering::Relaxed);
            apic::send_ipi_to_cpu(c, TLB_VECTOR);
        }
    }
    let deadline = cpu::rdtsc().saturating_add(WAIT_LIMIT_TSC);
    while PENDING.load(Ordering::Acquire) != 0 {
        cpu::pause();
        if cpu::rdtsc() > deadline {
            panic!("TLB shootdown timed out: CPU mask {:#x} never answered", PENDING.load(Ordering::Acquire));
        }
    }
    drop(_g);
    crate::hal::sync::irq_restore(was);
}
