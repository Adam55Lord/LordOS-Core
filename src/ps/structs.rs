// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Adam55Lord

//! The data structures of the Process Manager: `Process` (the NT `EPROCESS`)
//! and `Thread` (`ETHREAD`). Both are Object Manager objects, so they get
//! handles, access checks, reference counting and waiting for free.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use crate::hal::sync::IrqLock;
use crate::mm::vmm::{self, KernelStack};
use crate::ob::types::Event;
use crate::ob::{HandleTable, ObRef, Obj, ObjectType};

pub type Pid = u64;
pub type Tid = u64;

/// Priorities: 32 levels, higher runs first.
///
/// * `0` is the idle thread only.
/// * `1..=15` are *variable*: a thread that wakes from a wait gets a temporary
///   boost (never above 15) that decays by one every time it uses a full quantum.
/// * `16..=31` are *real-time*: fixed, never boosted.
pub mod priority {
    pub const IDLE: u8 = 0;
    pub const LOW: u8 = 4;
    pub const NORMAL: u8 = 8;
    pub const HIGH: u8 = 12;
    pub const MAX_VARIABLE: u8 = 15;
    #[allow(dead_code)]
    pub const REALTIME: u8 = 24;
    pub const MAX: u8 = 31;
    pub const LEVELS: usize = 32;
}

/// Scheduling state of a thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ThreadState {
    /// In a ready queue, waiting for the CPU.
    Ready = 1,
    /// On the CPU.
    Running = 2,
    /// Sleeping or waiting for an object.
    Waiting = 3,
    /// Finished; its resources go away with the last reference.
    Terminated = 4,
}

impl ThreadState {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => ThreadState::Ready,
            2 => ThreadState::Running,
            3 => ThreadState::Waiting,
            _ => ThreadState::Terminated,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            ThreadState::Ready => "ready",
            ThreadState::Running => "running",
            ThreadState::Waiting => "waiting",
            ThreadState::Terminated => "terminated",
        }
    }
}

/// How a wait ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitStatus {
    Signaled,
    TimedOut,
}

pub(super) const WAIT_NONE: u8 = 0;
pub(super) const WAIT_SIGNALED: u8 = 1;
pub(super) const WAIT_TIMEOUT: u8 = 2;

/// Where a user-mode thread starts.
#[derive(Clone, Copy, Debug)]
pub struct UserEntry {
    /// Program counter of the first user instruction.
    pub pc: u64,
    /// Initial user stack pointer.
    pub sp: u64,
    /// Passed in the first argument register of the architecture (the thread function's argument).
    pub arg: u64,
}

/// `EPROCESS`: an address space, a handle table and the threads that run in them.
pub struct Process {
    pub pid: Pid,
    pub name: String,
    pub parent: Pid,
    pub created_tick: u64,
    pub base_priority: u8,
    /// The user address space; `None` for the System process (kernel mappings only).
    pub(super) space: IrqLock<Option<vmm::AddressSpace>>,
    /// Physical address of the PML4 to load while this process runs (0 = kernel).
    pub(super) root: AtomicU64,
    /// This process's own handle table: what its threads can name.
    pub handles: HandleTable,
    pub(super) active_threads: AtomicUsize,
    pub(super) total_threads: AtomicUsize,
    pub(super) exited: AtomicBool,
    pub(super) exit_code: AtomicI32,
    pub(super) exit_code_set: AtomicBool,
    /// Next address `vm_alloc` hands out when the caller names none (bump allocator with guard gaps).
    pub vm_cursor: AtomicU64,
    /// Ranges `(va, pages)` the process got from `vm_alloc`: the only memory `vm_free` /
    /// `vm_protect` may touch (never its code, stack or shared-memory views).
    pub vm_allocs: IrqLock<Vec<(u64, u64)>>,
    /// Signaled when the last thread has ended: wait on the process to join it.
    pub exit_event: Obj<Event>,
}

impl Process {
    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }
    pub fn exit_code(&self) -> i32 {
        self.exit_code.load(Ordering::Acquire)
    }
    /// Threads that have not ended yet.
    pub fn thread_count(&self) -> usize {
        self.active_threads.load(Ordering::Acquire)
    }
    /// Threads ever created in it.
    pub fn total_threads(&self) -> usize {
        self.total_threads.load(Ordering::Relaxed)
    }
    pub fn is_user(&self) -> bool {
        self.root.load(Ordering::Relaxed) != 0
    }
    /// Runs `f` on the process's user address space. `None` for the System
    /// process and for a process that has ended (its space is gone).
    pub fn with_space<R>(&self, f: impl FnOnce(&mut vmm::AddressSpace) -> R) -> Option<R> {
        self.space.lock().as_mut().map(f)
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        PROCESSES.lock().remove(&self.pid);
    }
}

/// `ETHREAD`: a kernel stack, a saved context and scheduling state.
pub struct Thread {
    pub tid: Tid,
    /// The owning process (a strong reference: a thread keeps its process alive).
    pub process: Obj<Process>,
    pub name: String,
    pub(super) stack: IrqLock<Option<KernelStack>>,
    /// Kernel stack pointer while the thread is not running (see `lord_context_switch`).
    pub(super) saved_sp: AtomicU64,
    /// Value for the TSS `kstack_top` while this thread runs (where ring-3 interrupts land).
    pub(super) kstack_top: u64,
    pub(super) entry: fn(usize),
    pub(super) arg: usize,
    pub(super) user: Option<UserEntry>,
    pub(super) state: AtomicU8,
    pub(super) base_priority: AtomicU8,
    /// Base priority plus any temporary boost.
    pub(super) priority: AtomicU8,
    pub(super) quantum_left: AtomicU32,
    /// Tick at which a sleep or timed wait ends (0 = none).
    pub(super) wake_tick: AtomicU64,
    pub(super) wait_result: AtomicU8,
    /// The `Event` this thread is blocked on (address), or 0.
    pub(super) wait_event: AtomicUsize,
    /// The object a `wait` system call is blocked on. It lives here, not on the thread's
    /// stack, so a thread that is killed while blocked drops it with the thread.
    pub(super) held: IrqLock<Option<ObRef>>,
    pub created_tick: u64,
    /// Timer ticks this thread has spent on the CPU.
    pub(super) cpu_ticks: AtomicU64,
    /// How many times the scheduler switched *to* this thread.
    pub(super) switches_in: AtomicU64,
    pub(super) exit_code: AtomicI32,
    /// Signaled when the thread ends: wait on the thread to join it.
    pub exit_event: Obj<Event>,
    /// CPUs the thread may run on (bit `n` = logical CPU `n`); by default all of them.
    pub(super) affinity: AtomicU64,
    /// The CPU the thread runs on, or ran on last.
    pub(super) cpu: AtomicUsize,
    /// True from the moment a CPU picks the thread until that CPU has saved its stack pointer
    /// (cleared by `lord_context_switch`). Nobody else may resume a thread that is `on_cpu`.
    pub(super) on_cpu: AtomicBool,
    /// The CPU whose ready queue holds the thread, or `usize::MAX` if it is in none.
    pub(super) queued_on: AtomicUsize,
    /// The priority level of the queue it sits in (its `priority` may change meanwhile).
    pub(super) queued_prio: AtomicU8,
    /// Times the thread started running on another CPU than the one it ran on before.
    pub(super) migrations: AtomicU64,
}

impl Thread {
    /// Bit mask of the CPUs the thread may run on.
    pub fn affinity(&self) -> u64 {
        self.affinity.load(Ordering::Relaxed)
    }
    /// The logical CPU the thread runs on (or ran on last).
    pub fn last_cpu(&self) -> usize {
        self.cpu.load(Ordering::Relaxed)
    }
    /// How many times the scheduler moved the thread to another CPU.
    pub fn migrations(&self) -> u64 {
        self.migrations.load(Ordering::Relaxed)
    }
    pub fn state(&self) -> ThreadState {
        ThreadState::from_u8(self.state.load(Ordering::Acquire))
    }
    /// Atomically moves `from` -> `to`; false if the thread was in another state.
    pub(super) fn cas_state(&self, from: ThreadState, to: ThreadState) -> bool {
        self.state.compare_exchange(from as u8, to as u8, Ordering::AcqRel, Ordering::Acquire).is_ok()
    }
    /// Sets the state and returns the previous one.
    pub(super) fn swap_state(&self, s: ThreadState) -> ThreadState {
        ThreadState::from_u8(self.state.swap(s as u8, Ordering::AcqRel))
    }
    pub(super) fn set_state(&self, s: ThreadState) {
        self.state.store(s as u8, Ordering::Release);
    }
    pub fn priority(&self) -> u8 {
        self.priority.load(Ordering::Relaxed)
    }
    pub fn base_priority(&self) -> u8 {
        self.base_priority.load(Ordering::Relaxed)
    }
    pub fn cpu_ticks(&self) -> u64 {
        self.cpu_ticks.load(Ordering::Relaxed)
    }
    pub fn switches_in(&self) -> u64 {
        self.switches_in.load(Ordering::Relaxed)
    }
    pub fn exit_code(&self) -> i32 {
        self.exit_code.load(Ordering::Acquire)
    }
    pub fn is_user(&self) -> bool {
        self.user.is_some()
    }
}

impl Drop for Thread {
    fn drop(&mut self) {
        THREADS.lock().remove(&self.tid);
        if let Some(stack) = self.stack.lock().take() {
            // Never the stack we are running on: the scheduler only drops a
            // terminated thread after it has switched away (see `finish_switch`).
            let _ = vmm::free_stack(stack);
        }
    }
}

// ----------------------------------------------------------------------
// Registries (weak: they hold header addresses, removed in Drop)
// ----------------------------------------------------------------------

pub(super) static PROCESSES: IrqLock<BTreeMap<Pid, usize>> = IrqLock::new(BTreeMap::new());
pub(super) static THREADS: IrqLock<BTreeMap<Tid, usize>> = IrqLock::new(BTreeMap::new());

pub(super) static NEXT_PID: AtomicU64 = AtomicU64::new(0);
pub(super) static NEXT_TID: AtomicU64 = AtomicU64::new(0);

macro_rules! type_slot {
    ($slot:ident, $getter:ident) => {
        pub(super) static $slot: AtomicPtr<ObjectType> = AtomicPtr::new(core::ptr::null_mut());
        pub fn $getter() -> &'static ObjectType {
            let p = $slot.load(Ordering::Acquire);
            assert!(!p.is_null(), "process manager not initialised");
            // SAFETY: set once from an immortal type object.
            unsafe { &*p }
        }
    };
}

type_slot!(PROCESS_TYPE, process_type);
type_slot!(THREAD_TYPE, thread_type);

/// A point-in-time description of a thread, for logs and tools.
#[derive(Clone, Debug)]
pub struct ThreadInfo {
    pub tid: Tid,
    pub pid: Pid,
    pub name: String,
    pub state: ThreadState,
    pub priority: u8,
    pub base_priority: u8,
    pub cpu_ticks: u64,
    pub switches_in: u64,
    pub created_tick: u64,
    pub user: bool,
    /// The CPU it runs on or ran on last.
    pub cpu: usize,
    pub affinity: u64,
    /// Times it was moved to another CPU.
    pub migrations: u64,
}

/// A point-in-time description of a process.
#[derive(Clone, Debug)]
pub struct ProcessInfo {
    pub pid: Pid,
    pub parent: Pid,
    pub name: String,
    pub threads: usize,
    pub total_threads: usize,
    pub exited: bool,
    pub exit_code: i32,
    pub base_priority: u8,
    pub created_tick: u64,
    pub user: bool,
}
