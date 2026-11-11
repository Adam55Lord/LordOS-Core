// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Adam55Lord

//! Kernel context switching: saving one thread's callee-saved state on its stack, resuming
//! another's, laying out the stack of a brand-new thread, and telling the CPU which kernel stack
//! to land on for the next trap from user mode.

use super::tss;

// NOTE: every `global_asm!` block continues in the section the previous one left active, so code
// must always say `.text` explicitly (a block of read-only data may come right before this one).
core::arch::global_asm!(
    r#"
    .text
    .global lord_context_switch
lord_context_switch:
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    mov [rdi], rsp
    mov byte ptr [rdx], 0
    mov rsp, rsi
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret
"#
);

unsafe extern "C" {
    fn lord_context_switch(old_slot: *mut u64, new_sp: u64, old_on_cpu: *mut u8);
}

/// Saves the callee-saved registers on the current stack, stores the stack pointer in
/// `*old_slot`, **then** clears `*old_on_cpu` (from that moment another CPU may resume the old
/// thread), switches to `new_sp` and restores from there.
///
/// # Safety
/// Interrupts are off; `new_sp` was produced by a previous `switch` away from that thread (after
/// which its `on_cpu` flag was cleared) or by [`build_initial_stack`].
pub unsafe fn switch(old_slot: *mut u64, new_sp: u64, old_on_cpu: *mut u8) {
    // SAFETY: forwarded contract.
    unsafe { lord_context_switch(old_slot, new_sp, old_on_cpu) }
}

/// Lays out a fresh kernel stack so the first [`switch`] to it "returns" into `entry` with a
/// correctly aligned stack. Returns the initial stack pointer.
///
/// # Safety
/// `top` must be the 16-byte-aligned end of a mapped, writable, unused stack.
pub unsafe fn build_initial_stack(top: u64, entry: extern "C" fn() -> !) -> u64 {
    let ret_slot = top - 16;
    let sp = ret_slot - 48; // six callee-saved registers
    // SAFETY: the caller guarantees the memory is ours and mapped.
    unsafe {
        let p = sp as *mut u64;
        for i in 0..6 {
            p.add(i).write(0);
        }
        (ret_slot as *mut u64).write(entry as usize as u64);
        ((top - 8) as *mut u64).write(0); // fake return address: backtraces stop here
    }
    sp
}

/// The stack the CPU switches to on the next trap from user mode (and `syscall` entry) on
/// `cpu`. The scheduler calls this on every switch to the incoming thread's kernel stack top.
pub fn set_kernel_stack(cpu: usize, top: u64) {
    tss::set_kernel_stack(cpu, top);
}

/// The kernel stack top currently set for `cpu`.
pub fn kernel_stack(cpu: usize) -> u64 {
    tss::kernel_stack(cpu)
}
