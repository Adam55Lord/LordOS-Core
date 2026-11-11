// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Adam55Lord

//! x86_64 platform bring-up: the ordered, architecture-specific steps `kmain` performs between
//! the generic ones. `kmain` calls them in this order:
//!
//! 1. [`log_cpu_identity`]       CPUID vendor/brand/topology
//! 2. [`init_boot_cpu`]          PAT, GDT+TSS, per-CPU data, `syscall`, PIC off, IDT (+ self-tests)
//! 3. [`mmu_self_test`]          page-table walker self-tests (needs the PMM)
//! 4. [`init_interrupt_controller`]  Local APIC of the boot CPU (needs paging + heap)
//! 5. [`init_timers`]            reference clock (HPET, else PIT) and the LAPIC timer
//! 6. [`start_secondary_cpus`]   INIT-SIPI through the boot backend, per-CPU bring-up
//! 7. [`for_each_platform_device`] devices to publish in the Object Manager namespace
//!
//! A port provides the same seven functions with the same meaning: what happens inside is its
//! own business (GIC + generic timer + PSCI on arm64; PLIC + `stimecmp`/SBI + HSM on RISC-V).

use core::sync::atomic::{AtomicBool, Ordering};

use super::{apic, apic_timer, cpu, gdt, hpet, idt, paging, percpu, pic, smp, syscall, timer, tss};
use crate::boot::{BootInfo, HwCpuId};
use crate::hal::{MAX_CPUS, PlatformDevice};
use crate::mm;

/// Did the boot CPU's Local APIC come up? (The timers depend on it.)
static APIC_OK: AtomicBool = AtomicBool::new(false);

/// 1. Prints what CPUID says about the processor and its thread/core topology.
pub fn log_cpu_identity() {
    let cpu = cpu::CpuInfo::detect();
    debug!("cpu:  vendor ........... {}", cpu.vendor());
    debug!("cpu:  model ............ {}", cpu.brand());
    debug!("cpu:  ring before GDT .. {:?}", gdt::current_ring());
    let topology = cpu::Topology::detect();
    match (topology.threads_per_core, topology.logical_per_package) {
        (Some(threads), Some(logical)) => debug!(
            "cpu:  topology ......... {} thread(s)/core, {} logical CPU(s)/package (CPUID 0Bh)",
            threads, logical
        ),
        _ => debug!(
            "cpu:  topology ......... HTT={}, {} logical CPU(s)/package (legacy CPUID)",
            topology.htt, topology.legacy_logical_per_package
        ),
    }
}

/// 2. Brings up the boot CPU: GDT + TSS, per-CPU data, `syscall`, legacy PIC off, IDT, and
/// proves the IDT works with a breakpoint. Returns the boot CPU's hardware id (its APIC id).
pub fn init_boot_cpu(boot: &BootInfo) -> HwCpuId {
    let bsp_hw_id = boot
        .smp
        .as_ref()
        .map_or_else(|| u64::from(cpu::initial_apic_id()), |s| s.bsp_hw_id);

    paging::init_pat(); // same cache-type layout whichever bootloader ran
    gdt::init_cpu(0);
    percpu::init(0, bsp_hw_id as u32);
    syscall::init_cpu();
    debug!(
        "gdt:  loaded ............ base {:#x}, {} slots",
        gdt::table_base(0),
        gdt::GDT_ENTRIES
    );
    debug!(
        "gdt:  ring 0 code/data .. {:#06x} / {:#06x}",
        gdt::KERNEL_CODE.raw(),
        gdt::KERNEL_DATA.raw()
    );
    debug!(
        "gdt:  ring 3 code/data .. {:#06x} / {:#06x}",
        gdt::USER_CODE.raw(),
        gdt::USER_DATA.raw()
    );
    debug!(
        "tss:  loaded ............ selector {:#06x}, size {} bytes",
        gdt::current_tr(),
        tss::TSS_SIZE
    );
    debug!("tss:  rsp0 (ring3->0) ... {:#018x}", tss::kernel_stack(0));
    for (index, role) in [
        (tss::IST_DOUBLE_FAULT, "double fault"),
        (tss::IST_NMI, "NMI"),
        (tss::IST_MACHINE_CHECK, "machine check"),
        (tss::IST_DEBUG, "debug"),
    ] {
        match tss::ist_stack(0, index) {
            Some(top) => debug!("tss:  IST{} {:<13} {:#018x}", index, role, top),
            None => warn!("tss:  IST{} {} not set", index, role),
        }
    }
    match gdt::verify(0) {
        Ok(()) => debug!(
            "gdt:  self-test ......... passed (running in ring {})",
            gdt::current_ring().map_or(255, |r| r.number())
        ),
        Err(why) => error!("gdt:  self-test FAILED: {}", why),
    }

    pic::disable();
    let (imr1, imr2) = pic::masks();
    if pic::is_disabled() {
        debug!(
            "pic:  8259 PIC .......... OFF (remapped to 0x20-0x2F, IMR {:#04x}/{:#04x} = all masked)",
            imr1, imr2
        );
    } else {
        error!("pic:  8259 PIC still has unmasked lines: {:#04x}/{:#04x}", imr1, imr2);
    }
    idt::init();
    idt::load();
    debug!(
        "idt:  loaded ............ base {:#x}, {} gates",
        idt::table_base(),
        idt::VECTOR_COUNT
    );
    debug!(
        "idt:  IST gates ......... #DF->IST{} NMI->IST{} #MC->IST{} #DB->IST{}",
        idt::ist_for_vector(idt::VEC_DOUBLE_FAULT as usize),
        idt::ist_for_vector(idt::VEC_NMI as usize),
        idt::ist_for_vector(idt::VEC_MACHINE_CHECK as usize),
        idt::ist_for_vector(idt::VEC_DEBUG as usize)
    );

    // IDT self-test: raise a breakpoint; the CPU must enter our handler and
    // come back here.
    cpu::breakpoint();
    if idt::count(idt::VEC_BREAKPOINT) == 1 {
        debug!("idt:  self-test ......... int3 handled, execution resumed");
    } else {
        error!("idt:  self-test FAILED: breakpoint was not handled");
    }

    bsp_hw_id
}

/// 3. Page-table walker self-tests, including the 1 GiB -> 2 MiB -> 4 KiB split path.
pub fn mmu_self_test() {
    match paging::self_test() {
        Ok(splits) => debug!(
            "mmu:  self-test ......... map/translate/unmap ok ({} large page(s) split), {} frame(s) held by paging",
            splits,
            mm::pmm::stats().in_use_frames
        ),
        Err(why) => error!("mmu:  self-test FAILED: {}", why),
    }
    let held_before = mm::pmm::stats().in_use_frames;
    match paging::self_test_large_pages() {
        Ok(splits) => debug!(
            "mmu:  large pages ....... 1GiB->2MiB->4KiB split ok ({} splits), neighbours + attributes intact",
            splits
        ),
        Err(why) => error!("mmu:  large-page self-test FAILED: {}", why),
    }
    let held_after = mm::pmm::stats().in_use_frames;
    if held_after != held_before {
        warn!("mmu:  large-page test leaked {} frame(s)", held_after.wrapping_sub(held_before) as i64);
    }

}

/// 4. Local APIC of the boot CPU: read, unmount the bootloader alias, mount uncached, enable;
/// then a self-IPI proves delivery and acknowledgement.
pub fn init_interrupt_controller() {
    let ok = match apic::init_bsp() {
        Ok(()) => {
            if apic::self_ipi_test() {
                debug!(
                    "apic: self-test ......... self-IPI vector {:#04x} delivered and acknowledged (EOI)",
                    apic::SELF_TEST_VECTOR
                );
            } else {
                error!("apic: self-test FAILED: self-IPI was not delivered");
            }
            true
        }
        Err(e) => {
            error!("apic: initialisation FAILED: {:?}", e);
            false
        }
    };
    APIC_OK.store(ok, Ordering::Release);
}

/// 5. Time: ACPI -> HPET (PIT as fallback) is the reference clock; the LAPIC timer is
/// calibrated against it. Must precede the other CPUs (the PIT is a single shared device and
/// the HPET mapping may unmount an alias).
pub fn init_timers(boot: &BootInfo) {
    let apic_ok = APIC_OK.load(Ordering::Acquire);
    match timer::init(boot.rsdp_phys) {
        Some(src) => info!("timer: reference clock is the {}", src.name()),
        None => warn!("timer: no reference clock (HPET and PIT both unavailable)"),
    }
    if apic_ok {
        match apic_timer::init_bsp() {
            Ok(_) => match apic_timer::self_test() {
                Ok(t) if !t.degraded => debug!(
                    "apic: timer self-test ... one-shot fires once; periodic {} Hz: {} ticks in {} us (expected {})",
                    t.hz,
                    t.ticks,
                    t.window_ns / 1_000,
                    t.expected
                ),
                Ok(t) => warn!(
                    "apic: timer self-test ... works, but periodic {} Hz delivered only {} of {} ticks in {} us (best of {}): interrupt latency > period; busy host?",
                    t.hz,
                    t.ticks,
                    t.expected,
                    t.window_ns / 1_000,
                    t.attempts
                ),
                Err(why) => error!("apic: timer self-test FAILED: {}", why),
            },
            Err(e) => error!("apic: timer calibration FAILED: {:?}", e),
        }
    }
}

/// 6. Starts the other CPUs (hardware threads / cores) through the boot backend and waits for
/// them. `bsp_hw_id` is the value [`init_boot_cpu`] returned.
pub fn start_secondary_cpus(boot: &BootInfo, bsp_hw_id: HwCpuId) {
    let bsp_lapic_id = bsp_hw_id as u32;
    let topology = cpu::Topology::detect();
    match boot.smp.as_ref() {
        Some(s) => {
            info!(
                "smp:  {} logical CPU(s) found (BSP lapic id {})",
                s.cpus.len(),
                s.bsp_hw_id
            );
            match topology.locate(bsp_lapic_id) {
                Some(p) => debug!(
                    "smp:  cpu 0 (BSP) ....... lapic id {} (package {} core {} thread {})",
                    bsp_lapic_id, p.package, p.core, p.thread
                ),
                None => debug!("smp:  cpu 0 (BSP) ....... lapic id {}", bsp_lapic_id),
            }
            let report = smp::start(s);
            if report.skipped > 0 {
                warn!(
                    "smp:  {} CPU(s) left parked (MAX_CPUS = {})",
                    report.skipped,
                    MAX_CPUS
                );
            }
            if report.online == report.requested + 1 {
                info!("smp:  {}/{} CPU(s) online", report.online, report.found);
            } else {
                warn!(
                    "smp:  only {} of {} requested CPU(s) came online",
                    report.online - 1,
                    report.requested
                );
            }
        }
        None => warn!("smp:  bootloader gave no MP info; running on one CPU"),
    }

}

/// 7. The devices this platform has, for the Object Manager namespace (`\\Device\\...`).
pub fn for_each_platform_device(mut f: impl FnMut(PlatformDevice)) {
    f(PlatformDevice {
        path: "\\Device\\Serial0",
        class: "serial",
        description: "16550 UART (COM1), kernel log",
        base: u64::from(super::uart16550::COM1_BASE),
        size: 8,
        io_port: true,
    });
    f(PlatformDevice {
        path: "\\Device\\LocalApic",
        class: "interrupt",
        description: "local APIC of the boot CPU",
        base: apic::read_base().phys,
        size: 4096,
        io_port: false,
    });
    if let Some(h) = hpet::info() {
        f(PlatformDevice {
            path: "\\Device\\Hpet",
            class: "timer",
            description: "High Precision Event Timer",
            base: h.phys,
            size: 1024,
            io_port: false,
        });
    }
}
