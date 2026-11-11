// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Adam55Lord

//! Contract stubs shared by the skeleton ports (aarch64, riscv64).
//!
//! Every public item here is part of the architecture contract (see `arch/mod.rs` and
//! `docs/23-architecture-abstraction.md`) with the exact signature the generic kernel uses, and
//! a `todo!()` body. A port **replaces these one module at a time**: define the module in
//! `arch/<name>/` (an explicit `pub mod x;` there shadows the stub of the same name) and delete
//! nothing here. When no stub is left in use, fold what remains into the port and drop this file.
//!
//! The skeletons type-check (`ARCH=<name> ./build.sh` reaches the link step) but cannot boot:
//! the first `todo!()` the boot path reaches panics with the name of the missing piece.

#![allow(dead_code, unused_variables)]

/// Body of a stub that has not been written yet: panics naming the missing piece.
macro_rules! missing {
    ($what:literal) => {
        todo!(concat!("arch port: ", $what, " is not implemented yet"))
    };
}

pub mod percpu {
    /// Index (0 = boot CPU) of the calling CPU, read from the architecture's per-CPU register
    /// (GS base on x86_64, `tpidr_el1` on arm64, `tp` on RISC-V).
    pub fn current_index() -> usize {
        missing!("percpu::current_index")
    }
}

pub mod paging {
    /// Neutral page-table-entry flags. The values are the *kernel's* view: `map_page_in` and
    /// `protect_page_in` take them and translate to the hardware format, `translate_in` returns
    /// them back in `Mapping::entry`. (On x86_64 they happen to equal the hardware bits.)
    pub mod pte {
        pub const PRESENT: u64 = 1 << 0;
        pub const WRITABLE: u64 = 1 << 1;
        pub const USER: u64 = 1 << 2;
        /// Software flag: the frame came from the PMM for this mapping and goes back on unmap.
        pub const OWNED: u64 = 1 << 9;
        pub const NO_EXECUTE: u64 = 1 << 63;
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum CacheMode {
        WriteBack,
        WriteThrough,
        /// Device registers (Device-nGnRnE on arm64, PBMT=IO on RISC-V).
        Uncached,
        WriteCombining,
    }

    impl CacheMode {
        /// The entry bits (in the neutral [`pte`] encoding or arch-private bits that
        /// `map_page_in` understands) selecting this memory type.
        pub const fn pte_bits(self) -> u64 {
            0
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum PagingError {
        Unaligned,
        NotMapped,
        AlreadyMapped,
        LargePageInTheWay,
        OutOfFrames,
        WindowExhausted,
    }

    #[derive(Clone, Copy)]
    pub struct Mapping {
        pub phys: u64,
        pub page_size: u64,
        /// Leaf entry in the neutral [`pte`] encoding.
        pub entry: u64,
    }

    #[derive(Clone, Copy)]
    pub struct Unmapped {
        pub phys: u64,
        pub entry: u64,
        pub splits: u32,
    }

    /// Start of the virtual window for MMIO mappings: a top-level slot nothing else uses.
    pub const MMIO_BASE: u64 = 0xFFFF_C000_0000_0000;

    /// Per-CPU MMU setup that must match on every CPU (memory-attribute registers: PAT on
    /// x86_64, MAIR on arm64).
    pub fn init_pat() { missing!("paging::init_pat") }
    pub fn nx_enabled() -> bool { missing!("paging::nx_enabled") }
    pub fn translate(virt: u64) -> Option<Mapping> { missing!("paging::translate") }
    pub fn translate_in(root: u64, virt: u64) -> Option<Mapping> { missing!("paging::translate_in") }
    pub fn map_page_in(root: u64, virt: u64, phys: u64, flags: u64) -> Result<(), PagingError> { missing!("paging::map_page_in") }
    pub fn unmap_page_in_nf(root: u64, virt: u64) -> Result<Unmapped, PagingError> { missing!("paging::unmap_page_in_nf") }
    pub fn flush_range(root: u64, virt: u64, pages: u64) { missing!("paging::flush_range") }
    pub fn protect_page_in(root: u64, virt: u64, flags: u64) -> Result<u64, PagingError> { missing!("paging::protect_page_in") }
    /// Makes sure the top-level slot covering `virt` points to a lower-level table.
    pub fn ensure_root_slot(root: u64, virt: u64) -> Result<bool, PagingError> { missing!("paging::ensure_root_slot") }
    pub fn reap_tables_in(root: u64, virt: u64) -> u32 { missing!("paging::reap_tables_in") }
    /// A new address-space root that shares the kernel half of `kernel_root`.
    pub fn create_root(kernel_root: u64) -> Result<u64, PagingError> { missing!("paging::create_root") }
    pub fn destroy_root(root: u64) -> u64 { missing!("paging::destroy_root") }
    /// # Safety
    /// `root` maps the kernel (code, stacks, trap vectors).
    pub unsafe fn load_root(root: u64) { missing!("paging::load_root") }
    pub fn current_root() -> u64 { missing!("paging::current_root") }
    pub fn map_mmio(phys: u64, len: usize) -> Result<*mut u8, PagingError> { missing!("paging::map_mmio") }
}

pub mod tlb {
    #[derive(Clone, Copy, Default)]
    pub struct TlbStats {
        pub shootdowns: u64,
        pub local_only: u64,
        pub ipis: u64,
    }
    pub fn stats() -> TlbStats { TlbStats::default() }
    pub fn served_by(cpu: usize) -> u64 { 0 }
    /// Is `root` the active address space of some other CPU? (decides whether to send IPIs)
    pub fn root_loaded_elsewhere(root: u64) -> bool { missing!("tlb::root_loaded_elsewhere") }
    /// Answer a pending shoot-down request (called from spin loops).
    pub fn poll() {}
}

pub mod smp {
    pub fn online_cpus() -> usize { 1 }
    /// Bit `n` set = logical CPU `n` is online (bit 0 = boot CPU).
    pub fn online_mask() -> u64 { 1 }
    /// Stops every other CPU (panic path). `false` if another CPU is already doing it.
    pub fn stop_other_cpus() -> bool { true }
}

pub mod syscall {
    pub fn count() -> u64 { 0 }
    pub fn verify() -> Result<(), &'static str> { missing!("syscall::verify") }
}

pub mod trap {
    /// Saved register state of an interrupted context.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct TrapFrame {
        /// Port: the architecture's saved registers go here (see `arch/x86_64/idt.rs`).
        _regs: [u64; 0],
    }
    pub type TrapHandler = fn(&mut TrapFrame);

    impl TrapFrame {
        pub fn from_user(&self) -> bool { missing!("TrapFrame::from_user") }
        pub fn syscall_nr(&self) -> u64 { missing!("TrapFrame::syscall_nr") }
        pub fn syscall_args(&self) -> [u64; 6] { missing!("TrapFrame::syscall_args") }
        pub fn set_syscall_ret(&mut self, value: u64) { missing!("TrapFrame::set_syscall_ret") }
        pub fn trap_number(&self) -> u64 { missing!("TrapFrame::trap_number") }
        pub fn trap_error(&self) -> u64 { missing!("TrapFrame::trap_error") }
        pub fn pc(&self) -> u64 { missing!("TrapFrame::pc") }
        pub fn is_page_fault(&self) -> bool { missing!("TrapFrame::is_page_fault") }
    }

    /// Installs the system-call handler (`svc` on arm64, `ecall` on RISC-V).
    pub fn install_syscall_handler(handler: TrapHandler) { missing!("trap::install_syscall_handler") }
    pub fn set_user_fault_hook(hook: TrapHandler) { missing!("trap::set_user_fault_hook") }
    /// Faulting address of the last page fault (`FAR_EL1` / `stval`).
    pub fn fault_address() -> u64 { missing!("trap::fault_address") }
}

pub mod context {
    /// # Safety
    /// See the x86_64 implementation: interrupts off; `new_sp` from `build_initial_stack` or a
    /// previous `switch`; clear `*old_on_cpu` only after the old context is fully saved.
    pub unsafe fn switch(old_slot: *mut u64, new_sp: u64, old_on_cpu: *mut u8) { missing!("context::switch") }
    /// # Safety
    /// `top` is the aligned end of a mapped, writable, unused stack.
    pub unsafe fn build_initial_stack(top: u64, entry: extern "C" fn() -> !) -> u64 { missing!("context::build_initial_stack") }
    pub fn set_kernel_stack(cpu: usize, top: u64) { missing!("context::set_kernel_stack") }
    pub fn kernel_stack(cpu: usize) -> u64 { missing!("context::kernel_stack") }
}

pub mod usermode {
    #[derive(Clone, Copy)]
    pub struct UserProgram {
        pub name: &'static str,
        pub image: &'static [u8],
    }
    /// # Safety
    /// The thread's address space is loaded and maps `pc` executable and `sp` writable.
    pub unsafe fn enter(pc: u64, sp: u64, arg: u64) -> ! { missing!("usermode::enter") }
    /// `hello`, `fault`, `spin`, `pid`, `writer` (behaviour in `docs/23`), in the port's machine code.
    pub fn test_programs() -> [UserProgram; 5] { missing!("usermode::test_programs") }
}

pub mod ipi {
    use super::trap::TrapHandler;
    pub fn send_resched(cpu: usize) { missing!("ipi::send_resched") }
    pub fn send_resched_all_but_self() { missing!("ipi::send_resched_all_but_self") }
    pub fn set_resched_hook(hook: TrapHandler) { missing!("ipi::set_resched_hook") }
    pub fn resched_received(cpu: usize) -> u64 { 0 }
}

pub mod tick {
    use super::trap::TrapHandler;
    #[derive(Debug)]
    pub struct TickError;
    pub fn set_hook(hook: TrapHandler) { missing!("tick::set_hook") }
    pub fn start_periodic_hz(hz: u32) -> Result<(), TickError> { missing!("tick::start_periodic_hz") }
}

pub mod serial {
    use core::fmt;

    use crate::hal::sync::IrqLock;

    /// The debug UART (PL011 on arm64, NS16550 on RISC-V virt).
    pub struct Uart;
    impl fmt::Write for Uart {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            // Port: write the bytes to the UART. Silent until then so early logging does not panic.
            let _ = s;
            Ok(())
        }
    }
    pub static COM1: IrqLock<Uart> = IrqLock::new(Uart);
    pub fn init() -> bool { false }
}

pub mod bringup {
    use crate::boot::{BootInfo, HwCpuId};
    use crate::hal::PlatformDevice;

    /// 1. Log what the CPU identification registers say.
    pub fn log_cpu_identity() { missing!("bringup::log_cpu_identity") }
    /// 2. Boot CPU: exception vectors, per-CPU data, MMU attributes. Returns its hardware id.
    pub fn init_boot_cpu(boot: &BootInfo) -> HwCpuId { missing!("bringup::init_boot_cpu") }
    /// 3. Page-table walker self-tests.
    pub fn mmu_self_test() { missing!("bringup::mmu_self_test") }
    /// 4. Interrupt controller of the boot CPU (GIC / PLIC+IMSIC).
    pub fn init_interrupt_controller() { missing!("bringup::init_interrupt_controller") }
    /// 5. Reference clock and the per-CPU tick timer.
    pub fn init_timers(boot: &BootInfo) { missing!("bringup::init_timers") }
    /// 6. Start the other CPUs through the boot backend (PSCI `CPU_ON` / SBI HSM).
    pub fn start_secondary_cpus(boot: &BootInfo, bsp_hw_id: HwCpuId) { missing!("bringup::start_secondary_cpus") }
    /// 7. Platform devices for the Object Manager namespace.
    pub fn for_each_platform_device(f: impl FnMut(PlatformDevice)) {}
}
