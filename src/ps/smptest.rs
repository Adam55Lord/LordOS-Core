// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Adam55Lord

//! Boot-time self-test of multi-CPU scheduling. It runs in the boot-driver thread after the
//! single-CPU checks of `selftest`, on however many CPUs the machine has (with one CPU the
//! parallel parts degrade to trivial truths, so it passes everywhere).
//!
//! What it proves:
//!
//! * **Parallelism**: as many CPU-bound threads as CPUs end up running on *all* CPUs.
//! * **Affinity**: pinned threads never leave their CPU (even across sleeps and yields); a running
//!   thread is moved to the CPU its mask is changed to.
//! * **Work stealing**: threads piled up on one CPU spread to the idle ones.
//! * **Cross-CPU termination**: a thread running on another CPU is taken off it and its
//!   resources are freed only afterwards.
//! * **Cross-CPU wake-ups**: two threads ping-pong through events thousands of times (this is the
//!   test that hammers the `on_cpu` hand-off between CPUs).
//! * **TLB shootdown**, with a ring-3 program that increments a counter in a data page on another
//!   CPU: the moment `protect` / `unmap` returns, that CPU must not be able to store to the
//!   page any more. Without the shootdown the stale TLB entry keeps working and the test fails.
//! * **Kernel map/unmap storms** from all CPUs at once (shootdowns in flight while other CPUs
//!   spin on the VMM lock), leaving the books balanced.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use super::selftest::{balance, join, join_process, settle};
use super::sched;
use super::structs::*;
use super::user::{spawn_user_image, user_programs, Mapping};
use super::*;
use crate::arch::{percpu, tlb};
use crate::hal::MAX_CPUS;
use crate::mm::vmm::{self, Prot};
use crate::ob::types::{create_event, EventKind};
use crate::ob::ObjectAttributes;

macro_rules! ensure {
    ($c:expr, $m:expr) => {
        if !$c {
            return Err($m);
        }
    };
}

macro_rules! tr {
    ($e:expr, $m:expr) => {
        $e.map_err(|_| $m)?
    };
}

/// What [`run`] observed, for the boot log.
#[derive(Clone, Copy, Default)]
pub struct SmpTest {
    pub cpus: usize,
    /// Distinct CPUs the parallel busy threads ran on.
    pub busy_cpus: usize,
    pub pinned_checks: u32,
    pub moves: u32,
    pub stolen: u64,
    pub remote_kills: u32,
    pub pingpong_rounds: u32,
    pub tlb_rounds: u32,
    pub map_ops: u32,
    pub shootdowns: u64,
    pub tlb_ipis: u64,
    pub resched_ipis: u64,
}

fn cpus() -> Vec<usize> {
    (0..MAX_CPUS).filter(|&c| sched::active_mask() >> c & 1 != 0).collect()
}

fn spawn_on(name: &str, entry: fn(usize), arg: usize, prio: u8, mask: u64) -> Result<Obj<Thread>, PsError> {
    let sys = sched::with_current(|t| t.process.clone());
    sched::create_thread_affine(&sys, name, entry, arg, prio, None, mask)
}

fn me() -> usize {
    percpu::current_index()
}

/// Waits (sleeping in 1 ms steps) until `f` is true; false after `ms`.
fn wait_until(ms: u64, mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..ms {
        if f() {
            return true;
        }
        sleep_ms(1);
    }
    f()
}

// -- A: parallelism ---------------------------------------------------------

static STOP: AtomicBool = AtomicBool::new(false);
static SEEN: AtomicU64 = AtomicU64::new(0);
const ZERO: AtomicU64 = AtomicU64::new(0);
static PROGRESS: [AtomicU64; MAX_CPUS * 2] = [ZERO; MAX_CPUS * 2];

fn busy(i: usize) {
    while !STOP.load(Ordering::Relaxed) {
        SEEN.fetch_or(1 << me(), Ordering::Relaxed);
        PROGRESS[i].fetch_add(1, Ordering::Relaxed);
    }
}

// -- B: affinity -------------------------------------------------------------

static PIN_SEEN: [AtomicU64; MAX_CPUS] = [ZERO; MAX_CPUS];

fn pinned(c: usize) {
    for i in 0..40 {
        PIN_SEEN[c].fetch_or(1 << me(), Ordering::Relaxed);
        match i % 3 {
            0 => yield_now(),
            1 => sleep_ms(1),
            _ => {
                for _ in 0..2000 {
                    PIN_SEEN[c].fetch_or(1 << me(), Ordering::Relaxed);
                }
            }
        }
    }
    PIN_SEEN[c].fetch_or(1 << me(), Ordering::Relaxed);
}

static MOVER_CPU: AtomicUsize = AtomicUsize::new(usize::MAX);
static MOVER_BITS: AtomicU64 = AtomicU64::new(0);

fn mover(_: usize) {
    while !STOP.load(Ordering::Relaxed) {
        let c = me();
        MOVER_CPU.store(c, Ordering::Relaxed);
        MOVER_BITS.fetch_or(1 << c, Ordering::Relaxed);
    }
}

// -- D: killing a thread that runs on another CPU ---------------------------

static VICTIM_ITERS: AtomicU64 = AtomicU64::new(0);

fn victim(_: usize) {
    loop {
        VICTIM_ITERS.fetch_add(1, Ordering::Relaxed);
    }
}

// -- E: ping-pong through events --------------------------------------------

struct Ping {
    a: Obj<crate::ob::types::Event>,
    b: Obj<crate::ob::types::Event>,
    rounds: u32,
    done: AtomicU64,
}

fn ponger(arg: usize) {
    // SAFETY: `arg` came from `Arc::into_raw` in `run`, one reference for this thread.
    let p = unsafe { Arc::from_raw(arg as *const Ping) };
    for _ in 0..p.rounds {
        if sched::wait_event(&p.a, Some(sched::ms_to_ticks(3000))) != WaitStatus::Signaled {
            return;
        }
        p.done.fetch_add(1, Ordering::Relaxed);
        p.b.set();
    }
}

// -- G: kernel map/unmap storm -----------------------------------------------

static MAP_OPS: AtomicU64 = AtomicU64::new(0);
static MAP_BAD: AtomicU64 = AtomicU64::new(0);

fn mapper(id: usize) {
    for round in 0..120u64 {
        let pages = 1 + (round % 4);
        let Ok(p) = vmm::alloc(pages, Prot::READ_WRITE, "smp-test") else {
            MAP_BAD.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let words = (pages * 4096 / 8) as usize;
        let pat = ((id as u64) << 32) | round;
        let q = p as *mut u64;
        // SAFETY: `pages` freshly committed, writable pages.
        unsafe {
            for w in (0..words).step_by(61) {
                q.add(w).write_volatile(pat ^ w as u64);
            }
        }
        if round % 5 == 0 {
            yield_now(); // may resume on another CPU: the mapping must work there too
        }
        // SAFETY: as above.
        let ok = unsafe { (0..words).step_by(61).all(|w| q.add(w).read_volatile() == pat ^ w as u64) };
        if !ok || vmm::free(p).is_err() {
            MAP_BAD.fetch_add(1, Ordering::Relaxed);
        }
        MAP_OPS.fetch_add(1, Ordering::Relaxed);
    }
}

// -- F: TLB shootdown from ring 3 ---------------------------------------------

const DATA_VA: u64 = 0x50_0000;

fn writer_process(on_cpu: usize) -> Result<Obj<Process>, &'static str> {
    let progs = user_programs();
    let w = tr!(progs.iter().find(|p| p.name == "writer").ok_or(()), "no writer program");
    static ZEROS: [u8; 4096] = [0; 4096];
    let maps = [
        Mapping { va: USER_CODE_BASE, bytes: w.image, prot: Prot::READ_EXEC.user() },
        Mapping { va: DATA_VA, bytes: &ZEROS, prot: Prot::READ_WRITE.user() },
    ];
    let p = tr!(spawn_user_image("writer", &maps, USER_CODE_BASE, priority::NORMAL), "spawn writer");
    // Its thread starts anywhere; pin it where the test wants it.
    let thread = tr!(
        sched::thread_infos().into_iter().find(|t| t.pid == p.pid && t.user).ok_or(()),
        "writer thread missing"
    );
    let tref = tr!(find_thread(thread.tid).ok_or(()), "writer thread object missing");
    tr!(sched::set_affinity(&tref, 1 << on_cpu), "pin writer");
    Ok(p)
}

fn find_thread(tid: Tid) -> Option<crate::ob::object::ObRef> {
    let reg = structs::THREADS.lock();
    // SAFETY: registry entries are removed before their object is freed.
    reg.get(&tid).and_then(|&a| unsafe { crate::ob::object::ObRef::upgrade(a) })
}

/// The counter the writer maintains, read through the physical frame (never through the user
/// mapping, which may be gone).
fn counter(p: &Obj<Process>) -> Option<u64> {
    let phys = p.space.lock().as_ref()?.translate(DATA_VA)?;
    // SAFETY: the frame is mapped in the process's space and reachable through the HHDM.
    Some(unsafe { (crate::hal::memory::phys_to_virt(phys) as *const u64).read_volatile() })
}

/// One round: run the writer on `target` while we run elsewhere, then `restrict` its page and
/// check that it stopped storing the instant the call returned.
fn tlb_round(target: usize, unmap: bool) -> Result<(), &'static str> {
    let p = writer_process(target)?;
    ensure!(wait_until(500, || counter(&p).is_some_and(|c| c > 1000)), "the writer never started counting");
    if unmap {
        let freed = {
            let mut g = p.space.lock();
            tr!(g.as_mut().ok_or(()), "space gone").unmap(DATA_VA, 1)
        };
        ensure!(freed == Ok(1), "unmap of the data page");
        // The page is gone on every CPU: the next store faults and ends the process. A stale
        // translation would let it count on (into a freed frame) forever.
        ensure!(wait_until(1000, || p.has_exited()), "the writer kept storing after its page was unmapped (stale TLB entry)");
        ensure!(p.exit_code() == 0xC000_000Eu32 as i32, "the writer did not die of a page fault");
    } else {
        {
            let mut g = p.space.lock();
            tr!(tr!(g.as_mut().ok_or(()), "space gone").protect(DATA_VA, 1, Prot::READ.user()), "protect");
        }
        // protect() has returned: every CPU has flushed, so its next store must fault and end it
        // (a stale writable translation on its CPU would let it count on forever).
        ensure!(wait_until(1000, || p.has_exited()), "the writer survived a store to a read-only page");
        ensure!(p.exit_code() == 0xC000_000Eu32 as i32, "the writer did not die of a page fault");
    }
    join_process(&p)?;
    Ok(())
}

pub fn run() -> Result<SmpTest, &'static str> {
    let mut out = SmpTest::default();
    let cpu_list = cpus();
    let n = cpu_list.len();
    out.cpus = n;
    settle();
    let bal0 = balance();
    let stacks0 = vmm::stats().regions;
    let threads0 = thread_type().total_objects.load(Ordering::Relaxed);
    let procs0 = process_type().total_objects.load(Ordering::Relaxed);
    let stolen0: u64 = cpu_list.iter().map(|&c| sched::cpu_stats(c).stolen).sum();
    let ipi0: u64 = cpu_list.iter().map(|&c| sched::cpu_stats(c).ipis_received).sum();
    let tlb0 = tlb::stats();
    let driver = tr!(sched::current_thread().ok_or(()), "no current thread");
    let all = u64::MAX;

    // A) As many busy threads as CPUs run on all of them at once.
    {
        STOP.store(false, Ordering::SeqCst);
        SEEN.store(0, Ordering::SeqCst);
        let mut ts = Vec::new();
        for i in 0..n {
            ts.push(tr!(spawn_on("busy", busy, i, priority::NORMAL, all), "spawn busy"));
        }
        sleep_ms(250);
        STOP.store(true, Ordering::SeqCst);
        for t in &ts {
            join(t)?;
        }
        let seen = SEEN.load(Ordering::SeqCst);
        out.busy_cpus = seen.count_ones() as usize;
        ensure!(PROGRESS[..n].iter().all(|p| p.load(Ordering::Relaxed) > 0), "a busy thread never ran");
        if out.busy_cpus != n {
            error!(
                "ps:   smp test: {} busy threads ran on CPU mask {:#x} (progress {:?}); per-CPU: {:?}",
                n,
                seen,
                PROGRESS[..n].iter().map(|p| p.load(Ordering::Relaxed)).collect::<Vec<_>>(),
                cpu_list.iter().map(|&c| sched::cpu_stats(c)).collect::<Vec<_>>()
            );
        }
        ensure!(out.busy_cpus == n, "busy threads did not spread over every CPU");
    }

    // B) Affinity: pinned threads stay put; a running thread follows its mask.
    {
        for &c in &cpu_list {
            PIN_SEEN[c].store(0, Ordering::SeqCst);
        }
        let mut ts = Vec::new();
        for &c in &cpu_list {
            ts.push(tr!(spawn_on("pinned", pinned, c, priority::NORMAL, 1 << c), "spawn pinned"));
        }
        for t in &ts {
            join(t)?;
        }
        for &c in &cpu_list {
            ensure!(PIN_SEEN[c].load(Ordering::SeqCst) == 1 << c, "a pinned thread ran on another CPU");
            out.pinned_checks += 1;
        }
        ensure!(
            spawn_on("nowhere", pinned, 0, priority::NORMAL, 1 << 40).is_err(),
            "a mask without any existing CPU was accepted"
        );

        STOP.store(false, Ordering::SeqCst);
        MOVER_CPU.store(usize::MAX, Ordering::SeqCst);
        let m = tr!(spawn_on("mover", mover, 0, priority::NORMAL, all), "spawn mover");
        sleep_ms(20);
        for round in 0..(n * 2) {
            let k = cpu_list[(round * 3 + 1) % n];
            tr!(sched::set_affinity(m.untyped(), 1 << k), "set affinity");
            ensure!(wait_until(300, || MOVER_CPU.load(Ordering::Relaxed) == k), "a running thread did not move to the CPU of its new mask");
            MOVER_BITS.store(0, Ordering::SeqCst);
            sleep_ms(8);
            ensure!(MOVER_BITS.load(Ordering::SeqCst) == 1 << k, "a thread left the CPU it was pinned to");
            out.moves += 1;
        }
        ensure!(sched::set_affinity(m.untyped(), 0) == Err(PsError::InvalidArgument), "an empty affinity mask was accepted");
        STOP.store(true, Ordering::SeqCst);
        join(&m)?;
    }

    // C) Work stealing: 2n spinners piled onto CPU 0 spread out once they may move.
    {
        STOP.store(false, Ordering::SeqCst);
        SEEN.store(0, Ordering::SeqCst);
        let mut ts = Vec::new();
        for i in 0..(2 * n) {
            ts.push(tr!(spawn_on("pile", busy, i, priority::NORMAL, 1), "spawn pile"));
        }
        sleep_ms(40);
        ensure!(SEEN.load(Ordering::SeqCst) == 1, "pinned threads left CPU 0");
        for t in &ts {
            tr!(sched::set_affinity(t.untyped(), all), "unpin");
        }
        sleep_ms(150);
        STOP.store(true, Ordering::SeqCst);
        for t in &ts {
            join(t)?;
        }
        ensure!(SEEN.load(Ordering::SeqCst).count_ones() as usize == n, "piled-up threads did not spread to the idle CPUs");
    }

    // D) Killing a thread that is running on another CPU.
    if n > 1 {
        let other = cpu_list[n - 1];
        for _ in 0..3 {
            let v = tr!(spawn_on("victim", victim, 0, priority::NORMAL, 1 << other), "spawn victim");
            ensure!(
                wait_until(300, || v.state() == ThreadState::Running && v.last_cpu() == other),
                "the victim never ran on its CPU"
            );
            ensure!(sched::terminate_thread(v.untyped(), 77) == Ok(()), "terminate of a remote running thread");
            ensure!(v.state() == ThreadState::Terminated && v.exit_code() == 77, "state of a killed remote thread");
            ensure!(!v.on_cpu.load(Ordering::Acquire), "terminate returned while the thread was still on its CPU");
            let after = VICTIM_ITERS.load(Ordering::Relaxed);
            sleep_ms(10);
            ensure!(after > 0, "the victim never counted");
            ensure!(VICTIM_ITERS.load(Ordering::Relaxed) == after, "the killed thread kept running");
            out.remote_kills += 1;
        }
        // The CPU is still fine afterwards.
        let t = tr!(spawn_on("after-kill", pinned, other, priority::NORMAL, 1 << other), "spawn");
        join(&t)?;
    }

    // E) Cross-CPU wake-ups: ping-pong through events.
    {
        let rounds = 400u32;
        let ping = Arc::new(Ping {
            a: tr!(create_event(&ObjectAttributes::unnamed(), EventKind::Synchronization, false), "event"),
            b: tr!(create_event(&ObjectAttributes::unnamed(), EventKind::Synchronization, false), "event"),
            rounds,
            done: AtomicU64::new(0),
        });
        let t = tr!(spawn_on("pong", ponger, Arc::into_raw(ping.clone()) as usize, priority::NORMAL, all), "spawn pong");
        for _ in 0..rounds {
            ping.a.set();
            ensure!(
                sched::wait_event(&ping.b, Some(sched::ms_to_ticks(3000))) == WaitStatus::Signaled,
                "a wake-up across CPUs was lost"
            );
        }
        join(&t)?;
        ensure!(ping.done.load(Ordering::Relaxed) == rounds as u64, "ping-pong lost rounds");
        out.pingpong_rounds = rounds;
    }

    // F) TLB shootdown: the writer runs on another CPU while we change its page.
    if n > 1 {
        // Keep ourselves off the writer's CPU.
        let mine = cpu_list[0];
        tr!(sched::set_affinity(&driver, 1 << mine), "pin driver");
        for round in 0..8 {
            let target = cpu_list[1 + round % (n - 1)];
            tlb_round(target, round % 2 == 1)?;
            out.tlb_rounds += 1;
        }
        tr!(sched::set_affinity(&driver, all), "unpin driver");
    }

    // G) Map/unmap storms from every CPU at once.
    {
        MAP_OPS.store(0, Ordering::SeqCst);
        MAP_BAD.store(0, Ordering::SeqCst);
        let mut ts = Vec::new();
        for i in 0..n.max(2) {
            ts.push(tr!(spawn_on("mapper", mapper, i, priority::NORMAL, all), "spawn mapper"));
        }
        for t in &ts {
            join(t)?;
        }
        ensure!(MAP_BAD.load(Ordering::SeqCst) == 0, "a kernel mapping misbehaved under concurrent map/unmap");
        out.map_ops = MAP_OPS.load(Ordering::SeqCst) as u32;
        ensure!(vmm::verify().is_ok(), "VMM inconsistent after the storm");
    }

    // H) Everything is gone again; the books balance.
    drop(driver);
    settle();
    ensure!(thread_type().total_objects.load(Ordering::Relaxed) == threads0, "thread objects leaked");
    ensure!(process_type().total_objects.load(Ordering::Relaxed) == procs0, "process objects leaked");
    ensure!(vmm::stats().regions == stacks0, "kernel stacks leaked");
    ensure!(balance() == bal0, "frames leaked");

    out.stolen = cpu_list.iter().map(|&c| sched::cpu_stats(c).stolen).sum::<u64>() - stolen0;
    out.resched_ipis = cpu_list.iter().map(|&c| sched::cpu_stats(c).ipis_received).sum::<u64>() - ipi0;
    let t1 = tlb::stats();
    out.shootdowns = t1.shootdowns - tlb0.shootdowns;
    out.tlb_ipis = t1.ipis - tlb0.ipis;
    if n > 1 {
        ensure!(out.stolen > 0, "no thread was ever stolen by an idle CPU");
        ensure!(out.resched_ipis > 0, "no reschedule IPI was ever delivered");
        ensure!(out.shootdowns > 0 && out.tlb_ipis > 0, "no TLB shootdown ever reached another CPU");
    }
    Ok(out)
}
