// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Adam55Lord

//! Spin locks.
//!
//! * [`SpinLock`]: plain lock. Not interrupt-safe: never take it in code that
//!   an interrupt handler (or the preemptive scheduler's timer tick) can also
//!   run on the same CPU. Kernel globals therefore use [`IrqLock`].
//! * [`IrqLock`]: disables interrupts on the local CPU while held, then
//!   restores the previous state. Used by every kernel global (log, serial,
//!   console, PMM, VMM, heap, Object Manager, scheduler): with a preemptive
//!   scheduler a thread must never be switched out while holding a lock.

use core::cell::UnsafeCell;
use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// What a CPU does while it spins (on a lock, for a flag): answer a pending TLB-shootdown
/// request. A CPU that waits with interrupts masked cannot take the IPI, so without this a
/// shootdown initiator holding a lock could wait forever for a CPU that waits for that lock.
#[cfg(target_os = "none")]
#[inline(always)]
pub fn spin_hook() {
    crate::arch::irq::spin_hook();
}
#[cfg(not(target_os = "none"))]
#[inline(always)]
pub fn spin_hook() {}

pub struct SpinLock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

// SAFETY: access to `data` is serialised by `locked`.
unsafe impl<T: Send> Sync for SpinLock<T> {}
unsafe impl<T: Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self { locked: AtomicBool::new(false), data: UnsafeCell::new(value) }
    }

    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
            spin_hook();
        }
        SpinLockGuard { lock: self }
    }

    /// Forcibly releases the lock. Only for the panic path, where the lock
    /// holder may be the code that just panicked.
    ///
    /// # Safety
    /// Caller must ensure no other live guard will be used afterwards.
    pub unsafe fn force_unlock(&self) {
        self.locked.store(false, Ordering::Release);
    }
}

pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the guard proves we hold the lock.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the guard proves we hold the lock exclusively.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

/// A `static`-friendly cell for hardware-owned data (GDT, TSS, stacks) that
/// the CPU reads or writes behind Rust's back. Access is `unsafe` by design:
/// the caller must guarantee exclusivity (single core, interrupts off, or an
/// external lock).
pub struct StaticCell<T>(UnsafeCell<T>);

// SAFETY: every access goes through `get()` + `unsafe`, so the caller owns the
// aliasing guarantees.
unsafe impl<T: Send> Sync for StaticCell<T> {}

impl<T> StaticCell<T> {
    pub const fn new(value: T) -> Self {
        Self(UnsafeCell::new(value))
    }

    pub const fn get(&self) -> *mut T {
        self.0.get()
    }
}

/// Is the interrupt flag set on this CPU? (The backend knows which register that is.)
#[cfg(target_os = "none")]
pub use crate::arch::irq::enabled as interrupts_enabled;

/// Masks interrupts on this CPU and returns whether they were enabled
/// (pass the result to [`irq_restore`]).
#[cfg(target_os = "none")]
pub use crate::arch::irq::save as irq_save;

/// Re-enables interrupts if `was_enabled`.
#[cfg(target_os = "none")]
pub use crate::arch::irq::restore as irq_restore;

// Host builds (the PMM model test) have no interrupt flag to touch.
#[cfg(not(target_os = "none"))]
pub fn interrupts_enabled() -> bool {
    false
}
#[cfg(not(target_os = "none"))]
pub fn irq_save() -> bool {
    false
}
#[cfg(not(target_os = "none"))]
pub fn irq_restore(_was_enabled: bool) {}

/// A [`SpinLock`] that also masks interrupts on the local CPU while held.
///
/// The previous interrupt state is restored when the guard drops, so locks
/// nest correctly and code that runs with interrupts off stays that way.
pub struct IrqLock<T> {
    inner: SpinLock<T>,
}

impl<T> IrqLock<T> {
    pub const fn new(value: T) -> Self {
        Self { inner: SpinLock::new(value) }
    }

    pub fn lock(&self) -> IrqLockGuard<'_, T> {
        let was_enabled = irq_save();
        IrqLockGuard { guard: ManuallyDrop::new(self.inner.lock()), was_enabled }
    }

    /// See [`SpinLock::force_unlock`].
    ///
    /// # Safety
    /// Same contract as [`SpinLock::force_unlock`].
    pub unsafe fn force_unlock(&self) {
        unsafe { self.inner.force_unlock() }
    }
}

pub struct IrqLockGuard<'a, T> {
    guard: ManuallyDrop<SpinLockGuard<'a, T>>,
    was_enabled: bool,
}

impl<T> Deref for IrqLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for IrqLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T> Drop for IrqLockGuard<'_, T> {
    fn drop(&mut self) {
        // SAFETY: dropped exactly once, here; the lock is released *before*
        // interrupts come back, so a handler can never see it held by us.
        unsafe { ManuallyDrop::drop(&mut self.guard) };
        irq_restore(self.was_enabled);
    }
}
