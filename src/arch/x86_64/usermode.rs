// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Adam55Lord

//! Dropping to user mode and the built-in user test programs (x86_64 machine code).

use super::gdt;

/// Initial RFLAGS of a user thread: IF set, reserved bit 1.
const USER_RFLAGS: u64 = 0x202;

/// Drops the calling thread to user mode at `pc` with stack pointer `sp` and `arg` as the
/// first argument register. Never returns.
///
/// # Safety
/// The thread's address space is loaded and maps `pc` executable and `sp` writable; the
/// per-CPU kernel stack for traps from user mode is set (`context::set_kernel_stack`).
pub unsafe fn enter(pc: u64, sp: u64, arg: u64) -> ! {
    // SAFETY: forwarded contract.
    unsafe { gdt::enter_ring3(pc, sp, USER_RFLAGS, arg) }
}

/// A program image built into the kernel.
#[derive(Clone, Copy)]
pub struct UserProgram {
    pub name: &'static str,
    pub image: &'static [u8],
}

core::arch::global_asm!(
    r#"
    .section .rodata.lord_user, "a"

    # prints a line 3 times (yielding in between), then exit(42)
    .global lord_user_hello_start
    .global lord_user_hello_end
lord_user_hello_start:
    mov r12d, 3
2:
    mov eax, 2
    lea rdi, [rip + lord_user_hello_msg]
    mov esi, 30
    int 0x80
    mov eax, 1
    int 0x80
    dec r12d
    jnz 2b
    xor eax, eax
    mov edi, 42
    int 0x80
    ud2
lord_user_hello_msg:
    .ascii "user process running in ring 3"
lord_user_hello_end:

    # writes to address 0: a page fault in ring 3
    .global lord_user_fault_start
    .global lord_user_fault_end
lord_user_fault_start:
    xor eax, eax
    mov dword ptr [rax], 1
    ud2
lord_user_fault_end:

    # never gives up the CPU: only preemption (or termination) stops it
    .global lord_user_spin_start
    .global lord_user_spin_end
lord_user_spin_start:
3:
    jmp 3b
lord_user_spin_end:

    # counts forever in the data page at 0x500000 (used by the TLB-shootdown tests: its stores
    # stop the moment that page is made read-only or unmapped *on every CPU*)
    .global lord_user_writer_start
    .global lord_user_writer_end
lord_user_writer_start:
    mov eax, 0x500000
5:
    inc qword ptr [rax]
    jmp 5b
lord_user_writer_end:

    # remembers its pid in a register across a sleep, then exit(pid)
    .global lord_user_pid_start
    .global lord_user_pid_end
lord_user_pid_start:
    mov eax, 3
    int 0x80
    mov r12d, eax
    mov eax, 4
    mov edi, 20
    int 0x80
    mov edi, r12d
    xor eax, eax
    int 0x80
    ud2
lord_user_pid_end:
"#
);


unsafe extern "C" {
    static lord_user_hello_start: u8;
    static lord_user_hello_end: u8;
    static lord_user_fault_start: u8;
    static lord_user_fault_end: u8;
    static lord_user_spin_start: u8;
    static lord_user_spin_end: u8;
    static lord_user_writer_start: u8;
    static lord_user_writer_end: u8;
    static lord_user_pid_start: u8;
    static lord_user_pid_end: u8;
}

fn image(start: *const u8, end: *const u8) -> &'static [u8] {
    // SAFETY: both symbols delimit a read-only blob in the kernel image.
    unsafe { core::slice::from_raw_parts(start, end as usize - start as usize) }
}

/// The built-in test programs: `hello`, `fault`, `spin`, `pid`, `writer`.
///
/// * `hello`: prints a line 3 times (yielding in between), then `exit(42)`
/// * `fault`: stores to address 0 (a fault in user mode)
/// * `spin`: never gives up the CPU
/// * `writer`: counts forever in the data page at `0x500000`
/// * `pid`: remembers its pid across a 20 ms sleep, then `exit(pid)`
pub fn test_programs() -> [UserProgram; 5] {
    [
        UserProgram { name: "hello", image: image(&raw const lord_user_hello_start, &raw const lord_user_hello_end) },
        UserProgram { name: "fault", image: image(&raw const lord_user_fault_start, &raw const lord_user_fault_end) },
        UserProgram { name: "spin", image: image(&raw const lord_user_spin_start, &raw const lord_user_spin_end) },
        UserProgram { name: "pid", image: image(&raw const lord_user_pid_start, &raw const lord_user_pid_end) },
        UserProgram { name: "writer", image: image(&raw const lord_user_writer_start, &raw const lord_user_writer_end) },
    ]
}
