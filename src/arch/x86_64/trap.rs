// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Adam55Lord

//! Traps: the saved register state of an interrupted context, the system-call entry and the
//! user-fault hook. The generic kernel names no register: it uses the accessors below.

use super::idt::{self, InterruptFrame};

/// The register state an interrupt, exception or system call saved. Generic code only uses
/// the methods of this type; the fields are the backend's business.
pub type TrapFrame = InterruptFrame;

/// A function that handles one trap. It runs with interrupts masked.
pub type TrapHandler = fn(&mut TrapFrame);

/// The vector the `int 0x80` system-call gate uses (the `syscall` instruction is wired
/// separately in `syscall.rs`; both reach the same generic dispatcher).
pub const SYSCALL_VECTOR: u8 = 0x80;

impl InterruptFrame {
    /// Did the trap arrive while running in user mode?
    #[inline]
    pub fn from_user(&self) -> bool {
        self.from_user_mode()
    }

    /// The system-call number (the argument registers are described by [`Self::syscall_args`]).
    #[inline]
    pub fn syscall_nr(&self) -> u64 {
        self.rax
    }

    /// The six system-call arguments, in order.
    #[inline]
    pub fn syscall_args(&self) -> [u64; 6] {
        [self.rdi, self.rsi, self.rdx, self.r10, self.r8, self.r9]
    }

    /// Sets the value the user context sees as the result when it resumes.
    #[inline]
    pub fn set_syscall_ret(&mut self, value: u64) {
        self.rax = value;
    }

    /// Which exception or interrupt this is (the architecture's own numbering).
    #[inline]
    pub fn trap_number(&self) -> u64 {
        self.vector
    }

    /// The architecture's error/syndrome value for this trap (0 if it has none).
    #[inline]
    pub fn trap_error(&self) -> u64 {
        self.error_code
    }

    /// Program counter of the interrupted instruction.
    #[inline]
    pub fn pc(&self) -> u64 {
        self.rip
    }

    /// Is this trap a page fault?
    #[inline]
    pub fn is_page_fault(&self) -> bool {
        self.vector == idt::VEC_PAGE_FAULT as u64
    }
}

/// Installs the system-call handler: reached from `int 0x80` here and from the `syscall`
/// instruction (`syscall.rs`).
pub fn install_syscall_handler(handler: TrapHandler) {
    idt::register(SYSCALL_VECTOR, handler);
    idt::allow_from_user_mode(SYSCALL_VECTOR);
}

/// Installs the function that handles CPU exceptions raised in user mode. It must not return
/// normally to the faulting code.
pub fn set_user_fault_hook(hook: TrapHandler) {
    idt::set_user_fault_hook(hook);
}

/// The faulting address of the last page fault on this CPU.
pub fn fault_address() -> u64 {
    idt::fault_address()
}
