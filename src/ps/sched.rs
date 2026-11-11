// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Adam55Lord

//! The scheduler: per-CPU ready queues, context switching, preemption, load balancing,
//! sleeping and waiting.
//!
//! # Model
//!
//! * **Every CPU schedules.** Each logical CPU has its own set of 32 FIFO ready queues (one per
//!   priority level) behind its own spin lock, an *idle thread* (the boot context of that CPU,
//!   priority 0, only ever runs there) and a "current thread". The boot CPU adopts its idle thread
//!   in [`init_system`]; an application processor parks until [`start_scheduling`] and then does
//!   the same in [`ap_main`].
//! * **Preemptive, priority-driven round robin per CPU**, exactly as before: the highest-priority
//!   ready thread runs, equal priorities share the CPU in quanta of [`QUANTUM_TICKS`] ticks.
//!   Every CPU has its own timer (`arch::tick`; the LAPIC timer on x86_64) ([`TICK_HZ`]); its interrupt handler ([`on_tick`])
//!   charges the running thread and switches *from inside the interrupt*. The boot CPU also keeps
//!   the global clock ([`ticks`]) and wakes sleepers.
//! * **Placement** ([`pick_cpu`]): a thread that becomes ready goes to the CPU in its affinity
//!   mask with the least work (queued threads, plus one if the CPU is busy), preferring the CPU
//!   it ran on last (warm caches). If that CPU is running something less important (or is idle)
//!   it gets a reschedule IPI.
//! * **Load balancing**: a CPU that is about to idle *steals* a ready thread from the busiest
//!   queue it may take from ([`steal_from`], honouring affinity). Because wake-ups already go to
//!   the least loaded CPU, imbalance only builds up when threads finish.
//! * **CPU affinity**: [`set_affinity`] / [`create_thread_affine`]. A running thread whose new
//!   mask excludes its CPU is moved at the next reschedule point (an IPI forces one).
//! * **Boost / decay** is unchanged: a variable-priority thread that finishes a wait is boosted
//!   one level (max 15); each full quantum it burns decays the boost.
//!
//! # Context switch and the cross-CPU race
//!
//! `arch::context::switch(old_slot, new_sp, old_on_cpu)` pushes the callee-saved registers, stores
//! the stack pointer in `*old_slot`, **then** clears `*old_on_cpu`, loads `new_sp`, pops the registers and
//! returns. Two rules make switching safe when several CPUs run the scheduler:
//!
//! 1. A thread's saved stack pointer is only valid once `on_cpu` is false. Whoever wants to run (or
//!    enqueue) a thread that was just switched out on another CPU waits for that flag.
//! 2. The switching CPU keeps **its ready-queue lock held across the switch**; the thread that
//!    resumes releases it in [`finish_switch`]. A thread that was preempted is queued *before*
//!    the switch (so a thief could pick it) but its queue is locked until its stack pointer is
//!    saved: nobody can take it too early.
//!
//! # Locks (order: `BLOCKED` -> `RQ[n]`; never two `RQ` locks at once)
//!
//! * `RQ[n]`: CPU `n`'s ready queues. Taken with interrupts masked.
//! * `BLOCKED`: sleepers and event waiters (shared by all CPUs). Held while a woken thread is
//!   queued, hence it is taken *before* a queue lock.
//! * Per-CPU data (`LOCAL[n]`: current, idle, ...) is touched only by CPU `n` with interrupts
//!   masked, so it needs no lock.
//!
//! Every kernel lock masks interrupts (`IrqLock`), so a thread can never be preempted while
//! holding one, and spin loops answer TLB-shootdown requests (see `arch::tlb`).
//!
//! # Rules
//!
//! * Blocking calls must not be made while holding a lock.
//! * A terminated thread's stack is freed only after its CPU has left it: the old `current`
//!   reference is parked in `CpuLocal::prev` and dropped by [`finish_switch`] on the *next*
//!   thread's stack.
//! * No `ObRef` is kept in a local across a context switch (a thread that never resumes would
//!   leak it); the scheduler's own structures own the references.
//! * The kernel is soft-float, so there is no FPU/SSE state to switch.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use super::structs::*;
use super::{PsError, KSTACK_PAGES};
use crate::arch::trap::TrapFrame;
use crate::hal::sync::{self, IrqLock, SpinLock, StaticCell};
use crate::arch::{context, cpu, ipi, paging, percpu, smp, tick, tlb};
use crate::hal::MAX_CPUS;
use crate::mm::vmm;
use crate::ob::object::{self, ObRef};
use crate::ob::types::{create_event, Event, EventKind};
use crate::ob::{Obj, ObjectAttributes};

/// Scheduler clock rate (per CPU).
pub const TICK_HZ: u32 = 100;
/// Timer ticks a thread may run before equal-priority peers get the CPU.
pub const QUANTUM_TICKS: u32 = 2;

const NOT_QUEUED: usize = usize::MAX;

pub fn ms_to_ticks(ms: u64) -> u64 {
    (ms * TICK_HZ as u64).div_ceil(1000).max(1)
}

// ----------------------------------------------------------------------
// State
// ----------------------------------------------------------------------

/// One CPU's ready queues.
struct RunQueue {
    ready: [VecDeque<ObRef>; priority::LEVELS],
    bitmap: u32,
}

impl RunQueue {
    const fn new() -> Self {
        Self { ready: [const { VecDeque::new() }; priority::LEVELS], bitmap: 0 }
    }
}

static RQ: [SpinLock<RunQueue>; MAX_CPUS] = [const { SpinLock::new(RunQueue::new()) }; MAX_CPUS];

/// Threads that are not runnable: sleeping or waiting for an event. Shared by all CPUs.
struct Blocked {
    /// (wake tick, tid) -> thread; sleeps and timed waits.
    sleepers: BTreeMap<(u64, Tid), ObRef>,
    /// Threads blocked on an event: (thread, event address).
    waiters: Vec<(ObRef, usize)>,
}

static BLOCKED: IrqLock<Blocked> = IrqLock::new(Blocked { sleepers: BTreeMap::new(), waiters: Vec::new() });

/// Data only the owning CPU touches (with interrupts masked).
struct CpuLocal {
    current: Option<ObRef>,
    idle: Option<ObRef>,
    /// The thread this CPU switched away from last: kept alive until the switch is complete
    /// (its stack may be in use until then), then dropped by `finish_switch`.
    prev: Option<ObRef>,
    /// A preempted thread whose affinity no longer allows this CPU; queued elsewhere by
    /// `finish_switch` (a queue of another CPU must not be locked while ours is).
    deferred: Option<ObRef>,
}

impl CpuLocal {
    const fn new() -> Self {
        Self { current: None, idle: None, prev: None, deferred: None }
    }
}

static LOCAL: [StaticCell<CpuLocal>; MAX_CPUS] = [const { StaticCell::new(CpuLocal::new()) }; MAX_CPUS];

const ZERO_USIZE: AtomicUsize = AtomicUsize::new(0);
const ZERO_U8: AtomicU8 = AtomicU8::new(0);
const NO_FLAG: AtomicBool = AtomicBool::new(false);

/// Threads waiting in each CPU's ready queues (a hint for placement and stealing; exact when
/// read under the queue lock).
static LOAD: [AtomicUsize; MAX_CPUS] = [ZERO_USIZE; MAX_CPUS];
/// Priority of the thread each CPU is running (0 = idle).
static RUN_PRIO: [AtomicU8; MAX_CPUS] = [ZERO_U8; MAX_CPUS];
/// "Reschedule soon" per CPU.
static NEED_RESCHED: [AtomicBool; MAX_CPUS] = [NO_FLAG; MAX_CPUS];
/// CPUs that run the scheduler (have adopted their idle thread).
static ACTIVE: AtomicU64 = AtomicU64::new(0);

struct CpuCounters {
    switches: AtomicU64,
    ticks: AtomicU64,
    idle_ticks: AtomicU64,
    stolen: AtomicU64,
    ipis: AtomicU64,
}

const ZERO_COUNTERS: CpuCounters = CpuCounters {
    switches: AtomicU64::new(0),
    ticks: AtomicU64::new(0),
    idle_ticks: AtomicU64::new(0),
    stolen: AtomicU64::new(0),
    ipis: AtomicU64::new(0),
};
static COUNTERS: [CpuCounters; MAX_CPUS] = [ZERO_COUNTERS; MAX_CPUS];

static STARTED: AtomicBool = AtomicBool::new(false);
static TICKS: AtomicU64 = AtomicU64::new(0);
static SWITCHES: AtomicU64 = AtomicU64::new(0);
static PREEMPTIONS: AtomicU64 = AtomicU64::new(0);
static MIGRATIONS: AtomicU64 = AtomicU64::new(0);
static WAITER_COUNT: AtomicUsize = AtomicUsize::new(0);
static KERNEL_ROOT: AtomicU64 = AtomicU64::new(0);

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}
pub fn switches() -> u64 {
    SWITCHES.load(Ordering::Relaxed)
}
pub fn preemptions() -> u64 {
    PREEMPTIONS.load(Ordering::Relaxed)
}
/// Times a thread was moved to another CPU (stolen, re-placed after a wake-up, or pushed off a
/// CPU its affinity no longer allows), all CPUs together.
pub fn migrations() -> u64 {
    MIGRATIONS.load(Ordering::Relaxed)
}
pub fn is_started() -> bool {
    STARTED.load(Ordering::Acquire)
}

/// CPUs that currently run the scheduler.
pub fn active_cpus() -> usize {
    ACTIVE.load(Ordering::Acquire).count_ones() as usize
}

/// Bit mask of the CPUs that run the scheduler.
pub fn active_mask() -> u64 {
    ACTIVE.load(Ordering::Acquire)
}

fn is_active(cpu: usize) -> bool {
    ACTIVE.load(Ordering::Acquire) >> cpu & 1 != 0
}

/// The calling CPU. Only stable while interrupts are masked (or the thread is pinned).
fn this_cpu() -> usize {
    percpu::current_index()
}

fn thr_ptr(r: &ObRef) -> *const Thread {
    r.body::<Thread>(thread_type()).expect("not a thread object") as *const Thread
}

/// # Safety
/// `p` must point to a live thread body (the caller holds a reference to its object).
unsafe fn th<'a>(p: *const Thread) -> &'a Thread {
    // SAFETY: guaranteed by the caller.
    unsafe { &*p }
}

/// The per-CPU data of `cpu`.
///
/// # Safety
/// The caller runs on `cpu` with interrupts masked (or is the boot CPU initialising a parked AP).
unsafe fn local(cpu: usize) -> &'static mut CpuLocal {
    // SAFETY: exclusive by the contract above.
    unsafe { &mut *LOCAL[cpu].get() }
}

// ----------------------------------------------------------------------
// Context switch
// ----------------------------------------------------------------------

pub(super) use crate::arch::context::{build_initial_stack, switch as context_switch};

/// First code every new thread runs (reached by the `ret` of the first switch).
extern "C" fn thread_start() -> ! {
    finish_switch();
    cpu::enable_interrupts();
    let (entry, arg) = with_current(|t| (t.entry, t.arg));
    entry(arg);
    exit_current(0)
}

/// Loads `root` (0 = the kernel's) into the MMU (CR3 on x86_64) unless it is loaded already.
fn switch_address_space(root: u64) {
    let target = if root == 0 { KERNEL_ROOT.load(Ordering::Relaxed) } else { root };
    if paging::current_root() != target {
        // SAFETY: `target` is the kernel root or a live process's root table, both of which map the
        // kernel half (code, stacks, IDT).
        unsafe { paging::load_root(target) };
    }
}

/// Runs on a thread's own stack right after it got the CPU, interrupts still masked: releases
/// the ready-queue lock the switching-away context left held, places a thread that could not
/// stay on this CPU, and drops the thread this CPU just left (its stack is no longer in use).
fn finish_switch() {
    let me = this_cpu();
    // SAFETY: the previous context locked exactly this CPU's queue before switching.
    unsafe { RQ[me].force_unlock() };
    // SAFETY: we are on `me` with interrupts masked.
    let lc = unsafe { local(me) };
    let deferred = lc.deferred.take();
    let prev = lc.prev.take();
    if let Some(t) = deferred {
        MIGRATIONS.fetch_add(1, Ordering::Relaxed);
        make_ready(t);
    }
    drop(prev);
}

// ----------------------------------------------------------------------
// Ready queues and placement
// ----------------------------------------------------------------------

/// Queues `t` on `cpu`'s queue (which the caller has locked) and marks it ready.
fn enqueue_locked(rq: &mut RunQueue, cpu: usize, t: ObRef) {
    // SAFETY: `t` is a live thread object.
    let tt = unsafe { th(thr_ptr(&t)) };
    let p = tt.priority() as usize;
    tt.queued_prio.store(p as u8, Ordering::Relaxed);
    tt.queued_on.store(cpu, Ordering::Release);
    tt.set_state(ThreadState::Ready);
    rq.ready[p].push_back(t);
    rq.bitmap |= 1 << p;
    LOAD[cpu].fetch_add(1, Ordering::Relaxed);
}

/// Takes the best ready thread off `cpu`'s queue.
fn pick_locked(rq: &mut RunQueue, cpu: usize) -> Option<ObRef> {
    if rq.bitmap == 0 {
        return None;
    }
    let p = 31 - rq.bitmap.leading_zeros() as usize;
    let q = &mut rq.ready[p];
    let t = q.pop_front().expect("bitmap says non-empty");
    if q.is_empty() {
        rq.bitmap &= !(1 << p);
    }
    LOAD[cpu].fetch_sub(1, Ordering::Relaxed);
    // SAFETY: `t` is a live thread object.
    unsafe { th(thr_ptr(&t)) }.queued_on.store(NOT_QUEUED, Ordering::Release);
    Some(t)
}

/// Removes `t` from `cpu`'s queue (locked by the caller), if it is there.
fn remove_locked(rq: &mut RunQueue, cpu: usize, t: &ObRef) -> Option<ObRef> {
    // SAFETY: live thread object.
    let tt = unsafe { th(thr_ptr(t)) };
    let p = tt.queued_prio.load(Ordering::Relaxed) as usize;
    let q = &mut rq.ready[p];
    let pos = q.iter().position(|x| x.same_object(t))?;
    let r = q.remove(pos);
    if q.is_empty() {
        rq.bitmap &= !(1 << p);
    }
    LOAD[cpu].fetch_sub(1, Ordering::Relaxed);
    tt.queued_on.store(NOT_QUEUED, Ordering::Release);
    r
}

/// The CPU `t` should run on now: inside its affinity mask and among the CPUs that schedule, the
/// one with the least work; ties go to the CPU it ran on last.
fn pick_cpu(t: &Thread) -> usize {
    let active = ACTIVE.load(Ordering::Acquire);
    let mut mask = t.affinity.load(Ordering::Relaxed) & active;
    if mask == 0 {
        // Its mask names only CPUs that do not schedule (yet): run it anywhere rather than nowhere.
        mask = active;
    }
    let last = t.cpu.load(Ordering::Relaxed);
    let mut best = (usize::MAX, 0usize);
    for c in 0..MAX_CPUS {
        if mask >> c & 1 == 0 {
            continue;
        }
        let busy = (RUN_PRIO[c].load(Ordering::Relaxed) != 0) as usize;
        let cost = (LOAD[c].load(Ordering::Relaxed) * 2 + busy) * 2 + (c != last) as usize;
        if cost < best.0 {
            best = (cost, c);
        }
    }
    best.1
}

fn send_resched(target: usize) {
    COUNTERS[target].ipis.fetch_add(1, Ordering::Relaxed);
    ipi::send_resched(target);
}

/// After queueing a thread of priority `prio` on `target`: if that CPU is idle or runs something
/// less important, ask it to reschedule (an IPI if it is another CPU).
fn kick(target: usize, prio: u8) {
    if RUN_PRIO[target].load(Ordering::Relaxed) < prio {
        NEED_RESCHED[target].store(true, Ordering::Release);
        if target != this_cpu() {
            send_resched(target);
        }
    }
}

/// Makes `t` runnable on the CPU that suits it best. Interrupts may be on or off. Waits (with
/// interrupts masked, serving TLB requests) until a CPU that just switched away from the thread
/// has saved its stack pointer.
fn make_ready(t: ObRef) {
    let was = sync::irq_save();
    // SAFETY: we own a reference to the thread.
    let tt = unsafe { th(thr_ptr(&t)) };
    while tt.on_cpu.load(Ordering::Acquire) {
        cpu::pause();
        tlb::poll();
    }
    let target = pick_cpu(tt);
    let prio = tt.priority();
    {
        let mut rq = RQ[target].lock();
        enqueue_locked(&mut rq, target, t);
    }
    kick(target, prio);
    sync::irq_restore(was);
}

/// Takes a ready thread this CPU may run from the queue of `victim`.
fn steal_from(victim: usize, thief: usize) -> Option<ObRef> {
    let mut rq = RQ[victim].lock();
    let mut bm = rq.bitmap;
    while bm != 0 {
        let p = 31 - bm.leading_zeros() as usize;
        bm &= !(1 << p);
        let pos = rq.ready[p].iter().position(|t| {
            // SAFETY: queued threads are live.
            unsafe { th(thr_ptr(t)) }.affinity.load(Ordering::Relaxed) >> thief & 1 != 0
        });
        if let Some(pos) = pos {
            let t = rq.ready[p].remove(pos).expect("position is valid");
            if rq.ready[p].is_empty() {
                rq.bitmap &= !(1 << p);
            }
            LOAD[victim].fetch_sub(1, Ordering::Relaxed);
            // SAFETY: live thread object.
            unsafe { th(thr_ptr(&t)) }.queued_on.store(NOT_QUEUED, Ordering::Release);
            return Some(t);
        }
    }
    None
}

/// If this CPU has nothing to run (it is idle, or its thread just blocked) and another CPU has
/// threads waiting, takes one of them. Called *before* this CPU's own queue is locked.
fn steal_if_idle(me: usize) -> Option<ObRef> {
    if LOAD[me].load(Ordering::Relaxed) != 0 {
        return None;
    }
    let active = ACTIVE.load(Ordering::Acquire);
    if active & active.wrapping_sub(1) == 0 {
        return None; // one CPU schedules
    }
    // SAFETY: on `me`, interrupts masked.
    let lc = unsafe { local(me) };
    let cur = lc.current.as_ref()?;
    // SAFETY: `current` is owned by the scheduler.
    let running = unsafe { th(thr_ptr(cur)) }.state() == ThreadState::Running;
    let is_idle = lc.idle.as_ref().is_some_and(|i| i.same_object(cur));
    if running && !is_idle {
        return None; // keep running our own thread
    }
    for i in 1..MAX_CPUS {
        let v = (me + i) % MAX_CPUS;
        if active >> v & 1 == 0 || LOAD[v].load(Ordering::Relaxed) == 0 {
            continue;
        }
        if let Some(t) = steal_from(v, me) {
            COUNTERS[me].stolen.fetch_add(1, Ordering::Relaxed);
            MIGRATIONS.fetch_add(1, Ordering::Relaxed);
            return Some(t);
        }
    }
    None
}

/// Is there a ready thread waiting on any other CPU?
fn work_elsewhere(me: usize) -> bool {
    let active = ACTIVE.load(Ordering::Acquire);
    (0..MAX_CPUS).any(|c| c != me && active >> c & 1 != 0 && LOAD[c].load(Ordering::Relaxed) > 0)
}

fn wake_thread(t: ObRef, result: u8) {
    // SAFETY: live thread object.
    let tt = unsafe { th(thr_ptr(&t)) };
    tt.wait_result.store(result, Ordering::Release);
    tt.wait_event.store(0, Ordering::Release);
    tt.wake_tick.store(0, Ordering::Release);
    if tt.base_priority() < priority::MAX_VARIABLE + 1 {
        let boosted = (tt.base_priority() + 1).min(priority::MAX_VARIABLE);
        if boosted > tt.priority() {
            tt.priority.store(boosted, Ordering::Relaxed);
        }
    }
    tt.quantum_left.store(QUANTUM_TICKS, Ordering::Relaxed);
    make_ready(t);
}

fn drop_waiter(b: &mut Blocked, addr: usize) -> Option<ObRef> {
    let pos = b.waiters.iter().position(|(w, _)| w.addr() == addr)?;
    WAITER_COUNT.fetch_sub(1, Ordering::AcqRel);
    Some(b.waiters.remove(pos).0)
}

// ----------------------------------------------------------------------
// schedule()
// ----------------------------------------------------------------------

/// Picks the best thread for this CPU and switches to it (possibly the same one).
/// Callable from thread context and from interrupt handlers; never while holding a lock.
pub fn schedule() {
    if !STARTED.load(Ordering::Acquire) {
        return;
    }
    let was = sync::irq_save();
    let me = this_cpu();
    if !is_active(me) {
        sync::irq_restore(was);
        return;
    }
    // Everything queued from here on sets the flag again.
    NEED_RESCHED[me].store(false, Ordering::Release);
    // Look for work elsewhere *before* taking our own queue lock: queue locks are never nested.
    let stolen = steal_if_idle(me);

    let switch = {
        let mut rq = RQ[me].lock();
        // SAFETY: on `me`, interrupts masked.
        let lc = unsafe { local(me) };
        if let Some(t) = stolen {
            enqueue_locked(&mut rq, me, t);
        }
        let cur = lc.current.as_ref().expect("no current thread");
        let curp = thr_ptr(cur);
        // SAFETY: owned by the scheduler (`current`).
        let cur_t = unsafe { th(curp) };
        let cur_is_idle = cur.same_object(lc.idle.as_ref().expect("no idle thread"));
        if cur_t.state() == ThreadState::Running {
            cur_t.set_state(ThreadState::Ready);
            if !cur_is_idle {
                if cur_t.affinity.load(Ordering::Relaxed) >> me & 1 != 0 {
                    enqueue_locked(&mut rq, me, cur.clone());
                } else {
                    // Its mask no longer allows this CPU: another CPU's queue gets it after the switch.
                    lc.deferred = Some(cur.clone());
                }
            }
        }
        let next = match pick_locked(&mut rq, me) {
            Some(t) => t,
            None => lc.idle.clone().expect("no idle thread"),
        };
        let nextp = thr_ptr(&next);
        // SAFETY: `next` came from this scheduler's own structures.
        let next_t = unsafe { th(nextp) };
        next_t.quantum_left.store(QUANTUM_TICKS, Ordering::Relaxed);
        if nextp == curp {
            next_t.set_state(ThreadState::Running);
            drop(rq);
            None
        } else {
            if next_t.cpu.load(Ordering::Relaxed) != me {
                next_t.migrations.fetch_add(1, Ordering::Relaxed);
            }
            next_t.cpu.store(me, Ordering::Relaxed);
            next_t.on_cpu.store(true, Ordering::Release);
            next_t.set_state(ThreadState::Running);
            next_t.switches_in.fetch_add(1, Ordering::Relaxed);
            SWITCHES.fetch_add(1, Ordering::Relaxed);
            COUNTERS[me].switches.fetch_add(1, Ordering::Relaxed);
            RUN_PRIO[me].store(if next.same_object(lc.idle.as_ref().expect("idle")) { 0 } else { next_t.priority() }, Ordering::Relaxed);
            // The old `current` is parked (not dropped): its stack is still ours until the switch is
            // complete. `next`'s reference becomes `current`.
            debug_assert!(lc.prev.is_none());
            lc.prev = lc.current.replace(next);
            // The queue lock stays held across the switch; `finish_switch` releases it.
            core::mem::forget(rq);
            Some((curp, nextp))
        }
    };

    if let Some((old, new)) = switch {
        // SAFETY: both threads are owned by the scheduler's structures (`current`, `prev`, queues).
        let (old_t, new_t) = unsafe { (th(old), th(new)) };
        context::set_kernel_stack(me, new_t.kstack_top);
        switch_address_space(new_t.process.root.load(Ordering::Relaxed));
        // SAFETY: interrupts are off; `new_t.saved_sp` was produced by a previous switch away
        // from that thread (after which `on_cpu` was cleared, so no CPU still uses that stack) or
        // by `build_initial_stack`.
        unsafe {
            context_switch(
                old_t.saved_sp.as_ptr(),
                new_t.saved_sp.load(Ordering::Acquire),
                old_t.on_cpu.as_ptr().cast::<u8>(),
            )
        };
        // We are `old` again (some CPU switched back to us, maybe not the one we left).
        finish_switch();
    }
    sync::irq_restore(was);
}

/// If something more important became ready on this CPU and it is safe to switch here
/// (interrupts enabled, hence no kernel lock held), switch now; otherwise the next timer tick
/// (or IPI) does it.
pub fn preempt_check() {
    if STARTED.load(Ordering::Acquire) && sync::interrupts_enabled() {
        let want = {
            let was = sync::irq_save();
            let w = NEED_RESCHED[this_cpu()].swap(false, Ordering::AcqRel);
            sync::irq_restore(was);
            w
        };
        if want {
            schedule();
        }
    }
}

/// Gives up the rest of the quantum; equal-priority peers run first.
pub fn yield_now() {
    schedule();
}

// ----------------------------------------------------------------------
// The clock and the reschedule IPI
// ----------------------------------------------------------------------

/// Wakes the sleepers whose time has come (boot CPU, once per tick).
fn expire_sleepers(now: u64) {
    let mut b = BLOCKED.lock();
    while let Some(entry) = b.sleepers.first_entry() {
        if entry.key().0 > now {
            break;
        }
        let t = entry.remove();
        // SAFETY: live thread object.
        let was_waiting_on_event = unsafe { th(thr_ptr(&t)) }.wait_event.load(Ordering::Acquire) != 0;
        if was_waiting_on_event {
            drop_waiter(&mut b, t.addr());
            wake_thread(t, WAIT_TIMEOUT);
        } else {
            wake_thread(t, WAIT_SIGNALED);
        }
    }
}

/// Timer-interrupt hook (installed by [`super::start`]); runs on every CPU that schedules.
pub(super) fn on_tick(_frame: &mut TrapFrame) {
    if !STARTED.load(Ordering::Acquire) {
        return;
    }
    let me = this_cpu();
    if !is_active(me) {
        return;
    }
    COUNTERS[me].ticks.fetch_add(1, Ordering::Relaxed);
    if me == 0 {
        let now = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
        expire_sleepers(now);
    }
    let mut resched = false;
    {
        // SAFETY: interrupt handler on `me`, interrupts masked.
        let lc = unsafe { local(me) };
        let cur = lc.current.as_ref().expect("no current thread");
        let is_idle = cur.same_object(lc.idle.as_ref().expect("no idle thread"));
        // SAFETY: owned by the scheduler.
        let t = unsafe { th(thr_ptr(cur)) };
        t.cpu_ticks.fetch_add(1, Ordering::Relaxed);
        if is_idle {
            COUNTERS[me].idle_ticks.fetch_add(1, Ordering::Relaxed);
            if LOAD[me].load(Ordering::Relaxed) > 0 || work_elsewhere(me) {
                resched = true;
            }
        } else if t.quantum_left.fetch_sub(1, Ordering::Relaxed) <= 1 {
            // Quantum used up: decay any boost, let peers run.
            if t.priority() > t.base_priority() {
                t.priority.fetch_sub(1, Ordering::Relaxed);
            }
            resched = true;
        }
        RUN_PRIO[me].store(if is_idle { 0 } else { t.priority() }, Ordering::Relaxed);
        if NEED_RESCHED[me].swap(false, Ordering::AcqRel) {
            resched = true;
            PREEMPTIONS.fetch_add(1, Ordering::Relaxed);
        }
    }
    if resched {
        schedule();
    }
}

/// Reschedule IPI (installed by [`super::start`]): a thread was queued on this CPU, a thread must
/// leave it, or the CPU is idle and should look for work.
pub(super) fn on_resched_ipi(_frame: &mut TrapFrame) {
    if !STARTED.load(Ordering::Acquire) {
        return;
    }
    let me = this_cpu();
    if !is_active(me) {
        return;
    }
    let forced = NEED_RESCHED[me].swap(false, Ordering::AcqRel);
    // SAFETY: interrupt handler on `me`, interrupts masked.
    let lc = unsafe { local(me) };
    let cur = lc.current.as_ref().expect("no current thread");
    let is_idle = cur.same_object(lc.idle.as_ref().expect("no idle thread"));
    // SAFETY: owned by the scheduler.
    let running = unsafe { th(thr_ptr(cur)) }.state() == ThreadState::Running;
    if forced || is_idle || !running {
        if forced {
            PREEMPTIONS.fetch_add(1, Ordering::Relaxed);
        }
        schedule();
    }
}

// ----------------------------------------------------------------------
// Sleeping and waiting
// ----------------------------------------------------------------------

/// Blocks the calling thread for at least `ticks` timer ticks.
pub fn sleep_ticks(ticks: u64) {
    let ticks = ticks.max(1);
    let was = sync::irq_save();
    {
        let mut b = BLOCKED.lock();
        // SAFETY: on this CPU with interrupts masked.
        let cur = unsafe { local(this_cpu()) }.current.clone().expect("no current thread");
        let tp = thr_ptr(&cur);
        // SAFETY: owned by the scheduler (`current`), and by `cur` until moved below.
        let t = unsafe { th(tp) };
        let wake = TICKS.load(Ordering::Relaxed) + ticks;
        t.wake_tick.store(wake, Ordering::Release);
        t.wait_event.store(0, Ordering::Release);
        t.wait_result.store(WAIT_NONE, Ordering::Release);
        // A thread another CPU terminated meanwhile must not go to sleep: the CAS fails and the
        // `schedule()` below takes it off the CPU for good.
        if t.cas_state(ThreadState::Running, ThreadState::Waiting) {
            let tid = t.tid;
            b.sleepers.insert((wake, tid), cur);
        }
    }
    schedule();
    sync::irq_restore(was);
}

pub fn sleep_ms(ms: u64) {
    sleep_ticks(ms_to_ticks(ms));
}

/// Waits for `ev` to be signaled (a synchronization event is consumed), for at
/// most `timeout_ticks` ticks if given. Thread context only.
pub fn wait_event(ev: &Event, timeout_ticks: Option<u64>) -> WaitStatus {
    let was = sync::irq_save();
    let status = {
        let mut b = BLOCKED.lock();
        if ev.try_wait() {
            drop(b);
            sync::irq_restore(was);
            return WaitStatus::Signaled;
        }
        if timeout_ticks == Some(0) {
            drop(b);
            sync::irq_restore(was);
            return WaitStatus::TimedOut;
        }
        // SAFETY: on this CPU with interrupts masked.
        let cur = unsafe { local(this_cpu()) }.current.clone().expect("no current thread");
        let tp = thr_ptr(&cur);
        // SAFETY: owned by the scheduler.
        let t = unsafe { th(tp) };
        t.wait_result.store(WAIT_NONE, Ordering::Release);
        t.wait_event.store(ev as *const Event as usize, Ordering::Release);
        if t.cas_state(ThreadState::Running, ThreadState::Waiting) {
            let tid = t.tid;
            match timeout_ticks {
                Some(n) => {
                    let wake = TICKS.load(Ordering::Relaxed) + n;
                    t.wake_tick.store(wake, Ordering::Release);
                    b.sleepers.insert((wake, tid), cur.clone());
                }
                None => t.wake_tick.store(0, Ordering::Release),
            }
            b.waiters.push((cur, ev as *const Event as usize));
            WAITER_COUNT.fetch_add(1, Ordering::AcqRel);
        }
        tp
    };
    schedule();
    // SAFETY: we are this thread, running; its object is alive.
    let r = unsafe { th(status) }.wait_result.load(Ordering::Acquire);
    sync::irq_restore(was);
    if r == WAIT_TIMEOUT { WaitStatus::TimedOut } else { WaitStatus::Signaled }
}

/// Called by `Event::set`: wakes the threads blocked on `ev`.
pub fn event_signaled(ev: &Event) {
    if !STARTED.load(Ordering::Acquire) || WAITER_COUNT.load(Ordering::Acquire) == 0 {
        return;
    }
    let addr = ev as *const Event as usize;
    let was = sync::irq_save();
    {
        let mut b = BLOCKED.lock();
        let mut i = 0;
        while i < b.waiters.len() {
            if b.waiters[i].1 != addr {
                i += 1;
                continue;
            }
            // Notification events stay signaled; synchronization events are
            // consumed by the thread they release.
            if !ev.try_wait() {
                break;
            }
            let (t, _) = b.waiters.remove(i);
            WAITER_COUNT.fetch_sub(1, Ordering::AcqRel);
            // SAFETY: live thread object.
            let tt = unsafe { th(thr_ptr(&t)) };
            let wake = tt.wake_tick.load(Ordering::Acquire);
            if wake != 0 {
                b.sleepers.remove(&(wake, tt.tid));
            }
            wake_thread(t, WAIT_SIGNALED);
            if ev.kind == EventKind::Synchronization {
                break;
            }
        }
    }
    sync::irq_restore(was);
    preempt_check();
}

// ----------------------------------------------------------------------
// Creating processes and threads
// ----------------------------------------------------------------------

fn new_exit_event() -> Result<Obj<Event>, PsError> {
    create_event(&ObjectAttributes::unnamed(), EventKind::Notification, false).map_err(PsError::Ob)
}

/// Makes a process object (no threads yet). `space = None` means a kernel
/// process that runs on the kernel mappings only.
pub fn create_process(
    name: &str,
    parent: Pid,
    base_priority: u8,
    space: Option<vmm::AddressSpace>,
) -> Result<Obj<Process>, PsError> {
    let root = space.as_ref().map_or(0, |s| s.root());
    let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
    let p = Process {
        pid,
        name: name.into(),
        parent,
        created_tick: ticks(),
        base_priority,
        space: IrqLock::new(space),
        root: AtomicU64::new(root),
        handles: crate::ob::HandleTable::new(),
        active_threads: AtomicUsize::new(0),
        total_threads: AtomicUsize::new(0),
        exited: core::sync::atomic::AtomicBool::new(false),
        exit_code: core::sync::atomic::AtomicI32::new(0),
        exit_code_set: core::sync::atomic::AtomicBool::new(false),
        vm_cursor: AtomicU64::new(crate::ex::VM_BASE),
        vm_allocs: IrqLock::new(Vec::new()),
        exit_event: new_exit_event()?,
    };
    let obj = object::create(process_type(), &ObjectAttributes::unnamed(), p).map_err(PsError::Ob)?;
    PROCESSES.lock().insert(pid, obj.untyped().addr());
    Ok(obj)
}


fn new_thread_object(
    process: &Obj<Process>,
    name: &str,
    entry: fn(usize),
    arg: usize,
    prio: u8,
    user: Option<UserEntry>,
    stack: Option<vmm::KernelStack>,
    saved_sp: u64,
    kstack_top: u64,
    affinity: u64,
) -> Result<Obj<Thread>, PsError> {
    let tid = NEXT_TID.fetch_add(1, Ordering::Relaxed);
    let t = Thread {
        tid,
        process: process.clone(),
        name: name.into(),
        stack: IrqLock::new(stack),
        saved_sp: AtomicU64::new(saved_sp),
        kstack_top,
        entry,
        arg,
        user,
        state: core::sync::atomic::AtomicU8::new(ThreadState::Ready as u8),
        base_priority: core::sync::atomic::AtomicU8::new(prio),
        priority: core::sync::atomic::AtomicU8::new(prio),
        quantum_left: core::sync::atomic::AtomicU32::new(QUANTUM_TICKS),
        wake_tick: AtomicU64::new(0),
        wait_result: core::sync::atomic::AtomicU8::new(WAIT_NONE),
        wait_event: AtomicUsize::new(0),
        held: IrqLock::new(None),
        created_tick: ticks(),
        cpu_ticks: AtomicU64::new(0),
        switches_in: AtomicU64::new(0),
        exit_code: core::sync::atomic::AtomicI32::new(0),
        exit_event: new_exit_event()?,
        affinity: AtomicU64::new(affinity),
        cpu: AtomicUsize::new(0),
        on_cpu: AtomicBool::new(false),
        queued_on: AtomicUsize::new(NOT_QUEUED),
        queued_prio: AtomicU8::new(0),
        migrations: AtomicU64::new(0),
    };
    let obj = object::create(thread_type(), &ObjectAttributes::unnamed(), t).map_err(PsError::Ob)?;
    THREADS.lock().insert(tid, obj.untyped().addr());
    Ok(obj)
}

/// Creates a thread in `process` and makes it ready; it may run on any CPU. A kernel thread runs
/// `entry(arg)` and ends when it returns; a user thread (`user = Some`)
/// enters ring 3 at the given address.
pub fn create_thread(
    process: &Obj<Process>,
    name: &str,
    entry: fn(usize),
    arg: usize,
    prio: u8,
    user: Option<UserEntry>,
) -> Result<Obj<Thread>, PsError> {
    create_thread_affine(process, name, entry, arg, prio, user, u64::MAX)
}

/// [`create_thread`] with a CPU affinity mask (bit `n` = logical CPU `n`; bits of CPUs that do not
/// exist are ignored; a mask without any existing CPU is an error). The thread never runs
/// anywhere else, not even for its first instruction.
pub fn create_thread_affine(
    process: &Obj<Process>,
    name: &str,
    entry: fn(usize),
    arg: usize,
    prio: u8,
    user: Option<UserEntry>,
    affinity: u64,
) -> Result<Obj<Thread>, PsError> {
    if process.has_exited() {
        return Err(PsError::ProcessExited);
    }
    if prio == priority::IDLE || prio > priority::MAX {
        return Err(PsError::InvalidArgument);
    }
    let valid = smp::online_mask() | 1;
    if affinity & valid == 0 {
        return Err(PsError::InvalidArgument);
    }
    let stack = vmm::alloc_stack(KSTACK_PAGES).map_err(PsError::Vm)?;
    // SAFETY: `stack` is a fresh, mapped, zeroed kernel stack that nothing uses yet.
    let sp = unsafe { build_initial_stack(stack.top, thread_start) };
    let top = stack.top;
    let obj = new_thread_object(process, name, entry, arg, prio, user, Some(stack), sp, top, affinity & valid)?;
    process.active_threads.fetch_add(1, Ordering::AcqRel);
    process.total_threads.fetch_add(1, Ordering::Relaxed);
    make_ready(obj.untyped().clone());
    preempt_check();
    Ok(obj)
}

/// Creates the System process (pid 0) and turns the boot context of every CPU into its idle thread
/// (tid 0 is the boot CPU's; priority 0, pinned to its CPU). Only the boot CPU schedules until
/// [`ap_main`] runs on the others. Boot CPU, once, before [`super::start`]; the application
/// processors are parked and do not touch their per-CPU slot until `start_scheduling`.
pub(super) fn init_system() -> Result<(), PsError> {
    KERNEL_ROOT.store(vmm::kernel_root(), Ordering::Relaxed);
    let system = create_process("System", 0, priority::NORMAL, None)?;
    let cpus = smp::online_mask() | 1;
    for c in 0..MAX_CPUS {
        if cpus >> c & 1 == 0 {
            continue;
        }
        let name = if c == 0 { alloc::string::String::from("Idle") } else { alloc::format!("Idle/{c}") };
        // The boot context already has a stack: record it as idle's, with the TSS value in use now.
        let idle = new_thread_object(&system, &name, idle_entry, 0, priority::IDLE, None, None, 0, context::kernel_stack(c), 1 << c)?;
        // SAFETY: just created.
        let t = unsafe { th(thr_ptr(idle.untyped())) };
        t.set_state(ThreadState::Running);
        t.cpu.store(c, Ordering::Relaxed);
        t.on_cpu.store(true, Ordering::Relaxed);
        system.active_threads.fetch_add(1, Ordering::AcqRel);
        system.total_threads.fetch_add(1, Ordering::Relaxed);
        // SAFETY: CPU `c` is either us or parked and waiting for `STARTED`.
        let lc = unsafe { local(c) };
        lc.current = Some(idle.untyped().clone());
        lc.idle = Some(idle.into_untyped());
    }
    ACTIVE.fetch_or(1, Ordering::AcqRel);
    // The System process object stays alive through its idle threads.
    Ok(())
}

fn idle_entry(_: usize) {}

/// Turns scheduling on. The caller (the boot context, i.e. its CPU's idle thread) should then
/// enable interrupts and idle. Wakes the parked application processors.
pub(super) fn start_scheduling() {
    STARTED.store(true, Ordering::Release);
    ipi::send_resched_all_but_self();
}

/// What an application processor does once it is online (called by `arch::smp`): parks (taking
/// interrupts, so TLB shootdowns are answered) until the scheduler starts, then adopts its idle
/// thread, starts its timer and idles; work reaches it through its queue and IPIs.
#[allow(dead_code)] // called by the arch backend's secondary-CPU entry
pub fn ap_main(me: usize) -> ! {
    while !STARTED.load(Ordering::Acquire) {
        cpu::enable_and_halt();
    }
    cpu::disable_interrupts();
    RUN_PRIO[me].store(0, Ordering::Relaxed);
    ACTIVE.fetch_or(1 << me, Ordering::AcqRel);
    if let Err(e) = tick::start_periodic_hz(TICK_HZ) {
        warn!("ps:   cpu {}: no scheduler tick ({:?}); it only runs what IPIs bring", me, e);
    }
    super::idle_loop()
}

// ----------------------------------------------------------------------
// Looking at the running thread
// ----------------------------------------------------------------------

/// The calling thread's body pointer (valid for as long as the thread runs), if scheduling has a
/// current thread on this CPU.
fn current_ptr() -> Option<*const Thread> {
    let was = sync::irq_save();
    // SAFETY: on this CPU with interrupts masked.
    let p = unsafe { local(this_cpu()) }.current.as_ref().map(thr_ptr);
    sync::irq_restore(was);
    p
}

/// A new reference to the calling thread.
pub fn current_thread() -> Option<ObRef> {
    let was = sync::irq_save();
    // SAFETY: on this CPU with interrupts masked.
    let r = unsafe { local(this_cpu()) }.current.clone();
    sync::irq_restore(was);
    r
}

/// Parks `obj` in the calling thread (see `Thread::held`) until [`release_held`]; returns
/// the object it replaced, if any, for the caller to drop outside any lock.
pub fn hold_current(obj: ObRef) -> Option<ObRef> {
    with_current(|t| t.held.lock().replace(obj))
}

/// Takes back what [`hold_current`] parked. Drop the result outside any lock.
pub fn release_held() -> Option<ObRef> {
    with_current(|t| t.held.lock().take())
}

/// The calling thread's TID.
pub fn current_tid() -> Tid {
    // SAFETY: the calling thread is alive while it runs.
    current_ptr().map_or(0, |p| unsafe { th(p) }.tid)
}

/// The calling thread's PID.
pub fn current_pid() -> Pid {
    // SAFETY: as above.
    current_ptr().map_or(0, |p| unsafe { th(p) }.process.pid)
}

/// A new reference to the process of the calling thread.
pub fn current_process() -> Obj<Process> {
    with_current(|t| t.process.clone())
}

/// The handle table of the calling thread's process, as a raw pointer.
///
/// Valid for as long as the calling thread lives: a thread holds a strong
/// reference to its process, and the table is a field of it. Unlike
/// [`current_process`] this keeps no reference on the stack, so a thread that
/// is killed while blocked (a blocking system call) leaks nothing.
pub fn current_handles() -> *const crate::ob::HandleTable {
    with_current(|t| &t.process.handles as *const crate::ob::HandleTable)
}

/// Runs `f` on the calling thread's control block.
pub(super) fn with_current<R>(f: impl FnOnce(&Thread) -> R) -> R {
    let p = current_ptr().expect("no current thread");
    // SAFETY: the calling thread cannot be freed while it runs; the pointer stays valid even if
    // the thread moves to another CPU meanwhile.
    f(unsafe { th(p) })
}

// ----------------------------------------------------------------------
// Ending threads and processes
// ----------------------------------------------------------------------

/// Bookkeeping common to every way a thread ends: exit code, exit event,
/// process accounting. The thread must not be in any queue nor on any CPU.
fn thread_ended(t: &Thread, code: i32) {
    t.exit_code.store(code, Ordering::Release);
    t.exit_event.set();
    let p = &*t.process;
    if p.active_threads.fetch_sub(1, Ordering::AcqRel) == 1 {
        process_ended(p, code);
    }
}

/// The last thread of a process is gone: release what the process owns.
fn process_ended(p: &Process, last_code: i32) {
    if p.exit_code_set.load(Ordering::Acquire) == false {
        p.exit_code.store(last_code, Ordering::Release);
    }
    p.exited.store(true, Ordering::Release);
    p.handles.close_all();
    let space = p.space.lock().take();
    if space.is_some() {
        // Never destroy a space that is loaded: not on this CPU ...
        let root = p.root.load(Ordering::Relaxed);
        if paging::current_root() == root {
            switch_address_space(0);
        }
        // ... and not on another one, which may still be finishing its switch away from the last
        // thread of this process (it will load another root within a few instructions).
        while tlb::root_loaded_elsewhere(root) {
            cpu::pause();
            tlb::poll();
        }
        drop(space); // page tables and every owned user frame go back to the PMM
    }
    // Shared-memory views: the address space is gone, so the sections they pinned can go.
    crate::ipc::process_ended(p.pid);
    p.exit_event.set();
}

/// Ends the calling thread with `code`. Does not return. (Do not hold object
/// references in the caller: they would never be dropped.)
pub fn exit_current(code: i32) -> ! {
    sync::irq_save();
    // SAFETY: the current thread is alive until we leave its stack for good.
    let t = unsafe { th(with_current(|t| t as *const Thread)) };
    // If another CPU's `terminate_thread` claimed our end first it does the bookkeeping (once we
    // are off the CPU); we only have to leave.
    if t.swap_state(ThreadState::Terminated) != ThreadState::Terminated {
        thread_ended(t, code);
    }
    // A terminated thread is never queued again, so this does not come back.
    loop {
        schedule();
    }
}

/// Ends another thread. Fails for the calling thread (use [`exit_current`]).
///
/// A thread that is running on another CPU is told to leave it (IPI) and this call waits until
/// it has; so when it returns the thread is guaranteed to be off every CPU.
pub fn terminate_thread(target: &ObRef, code: i32) -> Result<(), PsError> {
    let tp = target.body::<Thread>(thread_type()).ok_or(PsError::NotAThread)? as *const Thread;
    // SAFETY: `target` is a reference to the thread.
    let t = unsafe { th(tp) };
    if t.base_priority() == priority::IDLE {
        return Err(PsError::InvalidArgument); // the idle threads are the CPUs' own
    }
    let mut removed: Vec<ObRef> = Vec::new();
    let was = sync::irq_save();
    // SAFETY: on this CPU with interrupts masked.
    if unsafe { local(this_cpu()) }.current.as_ref().is_some_and(|c| c.same_object(target)) {
        sync::irq_restore(was);
        return Err(PsError::IsCurrent);
    }
    let mut running_on = None;
    loop {
        let mut b = BLOCKED.lock();
        match t.state() {
            ThreadState::Terminated => {
                drop(b);
                sync::irq_restore(was);
                return Err(PsError::AlreadyTerminated);
            }
            ThreadState::Waiting => {
                // Waiting threads change state only under `BLOCKED`, which we hold.
                let wake = t.wake_tick.load(Ordering::Acquire);
                if wake != 0 {
                    removed.extend(b.sleepers.remove(&(wake, t.tid)));
                }
                if t.wait_event.load(Ordering::Acquire) != 0 {
                    removed.extend(drop_waiter(&mut b, target.addr()));
                }
                t.set_state(ThreadState::Terminated);
                break;
            }
            ThreadState::Ready => {
                let c = t.queued_on.load(Ordering::Acquire);
                if c != NOT_QUEUED {
                    let mut rq = RQ[c].lock(); // BLOCKED -> RQ, the allowed order
                    if t.queued_on.load(Ordering::Acquire) == c {
                        removed.extend(remove_locked(&mut rq, c, target));
                        t.set_state(ThreadState::Terminated);
                        break;
                    }
                }
                // Being queued or picked right now: look again.
            }
            ThreadState::Running => {
                if t.cas_state(ThreadState::Running, ThreadState::Terminated) {
                    running_on = Some(t.cpu.load(Ordering::Acquire));
                    break;
                }
            }
        }
        drop(b);
        cpu::pause();
        tlb::poll();
    }
    if let Some(c) = running_on {
        // Its CPU takes it off at its next reschedule (`schedule()` never requeues a thread that
        // is not `Running`). Wait until the stack pointer is saved: only then may its address
        // space be freed.
        NEED_RESCHED[c].store(true, Ordering::Release);
        if c != this_cpu() {
            send_resched(c);
        }
        while t.on_cpu.load(Ordering::Acquire) {
            cpu::pause();
            tlb::poll();
        }
    }
    thread_ended(t, code);
    sync::irq_restore(was);
    drop(removed); // the scheduler's references; the thread dies with the last one
    Ok(())
}

/// Ends every thread of `process` other than the caller and records `code`
/// as its exit code. If the caller belongs to the process, call
/// [`exit_current`] afterwards.
pub fn terminate_process_threads(process: &Obj<Process>, code: i32) {
    process.exit_code.store(code, Ordering::Release);
    process.exit_code_set.store(true, Ordering::Release);
    let pid = process.pid;
    let threads: Vec<ObRef> = {
        let reg = THREADS.lock();
        // SAFETY: entries are removed in `Thread::drop` before the memory is freed.
        reg.values().filter_map(|&a| unsafe { ObRef::upgrade(a) }).collect()
    };
    for t in threads {
        let tt = t.body::<Thread>(thread_type()).expect("thread");
        if tt.process.pid == pid {
            let _ = terminate_thread(&t, code); // the caller itself is skipped (IsCurrent)
        }
    }
}

/// Ends the calling thread's whole process (a fault, or `exit` from a
/// single-threaded program). Does not return.
pub fn exit_current_process(code: i32) -> ! {
    let proc = with_current(|t| t.process.clone());
    terminate_process_threads(&proc, code);
    drop(proc);
    exit_current(code)
}

/// Changes a thread's base priority (and drops any boost). Takes effect at once.
pub fn set_priority(thread: &ObRef, prio: u8) -> Result<(), PsError> {
    if prio == priority::IDLE || prio > priority::MAX {
        return Err(PsError::InvalidArgument);
    }
    let tp = thread.body::<Thread>(thread_type()).ok_or(PsError::NotAThread)? as *const Thread;
    // SAFETY: `thread` is a reference.
    let t = unsafe { th(tp) };
    if t.base_priority() == priority::IDLE {
        return Err(PsError::InvalidArgument);
    }
    let was = sync::irq_save();
    loop {
        match t.state() {
            ThreadState::Ready => {
                let c = t.queued_on.load(Ordering::Acquire);
                if c != NOT_QUEUED {
                    let mut rq = RQ[c].lock();
                    if t.queued_on.load(Ordering::Acquire) == c {
                        // Re-queue under the new priority, on the same CPU.
                        let r = remove_locked(&mut rq, c, thread).expect("queued thread is in its queue");
                        t.base_priority.store(prio, Ordering::Relaxed);
                        t.priority.store(prio, Ordering::Relaxed);
                        enqueue_locked(&mut rq, c, r);
                        drop(rq);
                        kick(c, prio);
                        break;
                    }
                }
                cpu::pause();
                tlb::poll();
            }
            ThreadState::Running => {
                t.base_priority.store(prio, Ordering::Relaxed);
                t.priority.store(prio, Ordering::Relaxed);
                // A running thread that lowered itself may now be outranked.
                let c = t.cpu.load(Ordering::Acquire);
                NEED_RESCHED[c].store(true, Ordering::Release);
                if c != this_cpu() {
                    send_resched(c);
                }
                break;
            }
            _ => {
                t.base_priority.store(prio, Ordering::Relaxed);
                t.priority.store(prio, Ordering::Relaxed);
                break;
            }
        }
    }
    sync::irq_restore(was);
    preempt_check();
    Ok(())
}

/// Restricts a thread to the CPUs in `mask` (bit `n` = logical CPU `n`; CPUs that do not exist
/// are ignored, an empty result is an error). A ready thread queued on a CPU the new mask excludes
/// is moved at once; a running one leaves its CPU at the next reschedule, which an IPI forces.
pub fn set_affinity(thread: &ObRef, mask: u64) -> Result<(), PsError> {
    let tp = thread.body::<Thread>(thread_type()).ok_or(PsError::NotAThread)? as *const Thread;
    // SAFETY: `thread` is a reference.
    let t = unsafe { th(tp) };
    if t.base_priority() == priority::IDLE {
        return Err(PsError::InvalidArgument);
    }
    let mask = mask & (smp::online_mask() | 1);
    if mask == 0 {
        return Err(PsError::InvalidArgument);
    }
    let was = sync::irq_save();
    t.affinity.store(mask, Ordering::Release);
    loop {
        match t.state() {
            ThreadState::Ready => {
                let c = t.queued_on.load(Ordering::Acquire);
                if c != NOT_QUEUED {
                    let mut rq = RQ[c].lock();
                    if t.queued_on.load(Ordering::Acquire) == c {
                        if mask >> c & 1 != 0 {
                            break; // stays where it is
                        }
                        let r = remove_locked(&mut rq, c, thread);
                        drop(rq);
                        if let Some(r) = r {
                            MIGRATIONS.fetch_add(1, Ordering::Relaxed);
                            make_ready(r);
                        }
                        break;
                    }
                }
                cpu::pause();
                tlb::poll();
            }
            ThreadState::Running => {
                let c = t.cpu.load(Ordering::Acquire);
                if mask >> c & 1 == 0 {
                    NEED_RESCHED[c].store(true, Ordering::Release);
                    if c != this_cpu() {
                        send_resched(c);
                    }
                }
                break;
            }
            _ => break, // waiting: placed according to the mask when it wakes
        }
    }
    sync::irq_restore(was);
    preempt_check();
    Ok(())
}

// ----------------------------------------------------------------------
// Inspection
// ----------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct SchedStats {
    pub ticks: u64,
    pub switches: u64,
    pub preemptions: u64,
    pub migrations: u64,
    pub ready: usize,
    pub sleeping: usize,
    pub waiting: usize,
    pub processes: usize,
    pub threads: usize,
    /// CPUs that run the scheduler.
    pub cpus: usize,
}

pub fn stats() -> SchedStats {
    let (sleeping, waiting) = {
        let b = BLOCKED.lock();
        (b.sleepers.len(), b.waiters.len())
    };
    let ready = LOAD.iter().map(|l| l.load(Ordering::Relaxed)).sum();
    SchedStats {
        ticks: ticks(),
        switches: switches(),
        preemptions: preemptions(),
        migrations: migrations(),
        ready,
        sleeping,
        waiting,
        processes: PROCESSES.lock().len(),
        threads: THREADS.lock().len(),
        cpus: active_cpus(),
    }
}

/// What one CPU has done.
#[derive(Clone, Copy, Debug, Default)]
pub struct CpuStats {
    pub active: bool,
    /// Context switches *into* a thread on this CPU.
    pub switches: u64,
    /// Timer ticks this CPU took.
    pub ticks: u64,
    /// Of those, ticks it spent idle.
    pub idle_ticks: u64,
    /// Threads it took from other CPUs' queues.
    pub stolen: u64,
    /// Reschedule IPIs it sent.
    pub ipis_sent: u64,
    /// Reschedule IPIs it received.
    pub ipis_received: u64,
    /// Threads queued on it right now.
    pub ready: usize,
}

pub fn cpu_stats(c: usize) -> CpuStats {
    if c >= MAX_CPUS {
        return CpuStats::default();
    }
    CpuStats {
        active: is_active(c),
        switches: COUNTERS[c].switches.load(Ordering::Relaxed),
        ticks: COUNTERS[c].ticks.load(Ordering::Relaxed),
        idle_ticks: COUNTERS[c].idle_ticks.load(Ordering::Relaxed),
        stolen: COUNTERS[c].stolen.load(Ordering::Relaxed),
        ipis_sent: COUNTERS[c].ipis.load(Ordering::Relaxed),
        ipis_received: ipi::resched_received(c),
        ready: LOAD[c].load(Ordering::Relaxed),
    }
}

pub fn thread_infos() -> Vec<ThreadInfo> {
    let refs: Vec<ObRef> = {
        let reg = THREADS.lock();
        // SAFETY: registry entries are removed before their object is freed.
        reg.values().filter_map(|&a| unsafe { ObRef::upgrade(a) }).collect()
    };
    refs.iter()
        .filter_map(|r| r.body::<Thread>(thread_type()))
        .map(|t| ThreadInfo {
            tid: t.tid,
            pid: t.process.pid,
            name: t.name.clone(),
            state: t.state(),
            priority: t.priority(),
            base_priority: t.base_priority(),
            cpu_ticks: t.cpu_ticks(),
            switches_in: t.switches_in(),
            created_tick: t.created_tick,
            user: t.is_user(),
            cpu: t.last_cpu(),
            affinity: t.affinity(),
            migrations: t.migrations(),
        })
        .collect()
}

pub fn process_infos() -> Vec<ProcessInfo> {
    let refs: Vec<ObRef> = {
        let reg = PROCESSES.lock();
        // SAFETY: registry entries are removed before their object is freed.
        reg.values().filter_map(|&a| unsafe { ObRef::upgrade(a) }).collect()
    };
    refs.iter()
        .filter_map(|r| r.body::<Process>(process_type()))
        .map(|p| ProcessInfo {
            pid: p.pid,
            parent: p.parent,
            name: p.name.clone(),
            threads: p.thread_count(),
            total_threads: p.total_threads(),
            exited: p.has_exited(),
            exit_code: p.exit_code(),
            base_priority: p.base_priority,
            created_tick: p.created_tick,
            user: p.is_user(),
        })
        .collect()
}
