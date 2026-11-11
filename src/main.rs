// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Adam55Lord

//! Lord Core: a portable hybrid kernel by Adam55Lord (x86_64 today; see `src/arch/`).
//!
//! Boot flow:  firmware -> bootloader -> `boot::<backend>::_start` -> [`kmain`].
//! The boot backend (Lord Boot today, see `src/boot/`) loads this kernel (an LKF image, `kernel.lkf`) into the
//! higher half, brings the CPU to 64-bit mode with a stack and a framebuffer,
//! translates what the bootloader knows into a bootloader-independent
//! [`boot::BootInfo`] and calls `kmain`. Nothing below this file names a
//! bootloader.

#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
mod log;

mod arch;
mod boot;
mod ex;
mod ld;
mod hal;
mod ipc;
mod mm;
mod ob;
mod ps;

use core::panic::PanicInfo;

use boot::BootInfo;

const NAME: &str = "Lord Core";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const AUTHOR: &str = "Adam55Lord";

/// Everything the kernel drives is an object: describe the CPUs and the
/// devices found during boot in the namespace (`\\Processors`, `\\Device`).
fn register_kernel_objects(boot: &BootInfo, bsp_hw_id: boot::HwCpuId) {
    use ob::types::{create_device, create_processor, Device, Processor};
    use ob::ObjectAttributes;

    let mut n_cpu = 0;
    match boot.smp.as_ref() {
        Some(s) => {
            // Logical numbering as in arch::smp: BSP is 0, the others follow in list order.
            let mut next = 1usize;
            for c in s.cpus {
                let (index, bsp) = if c.hw_id == bsp_hw_id {
                    (0, true)
                } else {
                    next += 1;
                    (next - 1, false)
                };
                if index >= hal::MAX_CPUS {
                    continue;
                }
                let path = alloc::format!("\\Processors\\CPU{}", index);
                if create_processor(&ObjectAttributes::named(&path).permanent(), Processor { index, hw_id: c.hw_id, bsp }).is_ok() {
                    n_cpu += 1;
                }
            }
        }
        None => {
            let p = Processor { index: 0, hw_id: bsp_hw_id, bsp: true };
            if create_processor(&ObjectAttributes::named("\\Processors\\CPU0").permanent(), p).is_ok() {
                n_cpu = 1;
            }
        }
    }

    let dev = |path: &str, class, description, base, size, io_port| {
        let d = Device { class, description, base, size, io_port };
        if let Err(e) = create_device(&ObjectAttributes::named(path).permanent(), d) {
            warn!("ob:   could not register {}: {:?}", path, e);
        }
    };
    if let Some(fb) = boot.framebuffer {
        dev("\\Device\\Framebuffer", "display", "linear framebuffer, text console", fb.address as u64, (fb.pitch * fb.height) as u64, false);
    }
    // The rest is whatever the platform has (UART, interrupt controller, timers, ...).
    arch::bringup::for_each_platform_device(|d| dev(d.path, d.class, d.description, d.base, d.size, d.io_port));
    debug!("ob:   registered {} processor object(s) and the boot devices", n_cpu);
}

fn human(bytes: u64) -> (u64, &'static str) {
    match bytes {
        b if b >= 1 << 30 => (b >> 30, "GiB"),
        b if b >= 1 << 20 => (b >> 20, "MiB"),
        b if b >= 1 << 10 => (b >> 10, "KiB"),
        b => (b, "B"),
    }
}

/// Kernel entry, called by the boot backend with a complete [`BootInfo`].
pub fn kmain(boot: &BootInfo) -> ! {
    // 1. HAL first: gives the logger its output devices (serial + screen).
    let status = hal::init(boot.framebuffer);

    // 2. Who we are.
    info!("{} v{} ({})", NAME, VERSION, arch::NAME);
    info!("Created by {}", AUTHOR);

    // 3. What the HAL brought up.
    debug!("hal: serial COM1 ........ {}", if status.serial { "ok" } else { "not found" });
    match status.display {
        Some(g) => debug!(
            "hal: framebuffer ....... {}x{}x{} -> {}x{} text cells (x{} scale)",
            g.width, g.height, g.bpp, g.cols, g.rows, g.scale
        ),
        None => warn!("hal: framebuffer ....... unavailable (serial output only)"),
    }

    // 4. What the bootloader told us (already translated by the boot backend).
    match boot.bootloader {
        Some(b) => {
            debug!("boot: bootloader ........ {} {}", b.name, b.version);
            debug!(
                "boot: protocol .......... {}, revision requested {:?}, loaded {:?}",
                b.protocol, b.revision_wanted, b.revision_loaded
            );
        }
        None => warn!("boot: bootloader info missing"),
    }
    match boot.firmware {
        Some(fw) => debug!("boot: firmware .......... {}", fw.name()),
        None => warn!("boot: firmware type missing"),
    }

    // 5. CPU identity and thread/core topology.
    arch::bringup::log_cpu_identity();

    // 5b. Memory: HHDM + physical memory map (paging and the APIC need them).
    match (boot.hhdm_offset, boot.memory_map.is_empty()) {
        (Some(hhdm), false) => {
            hal::memory::init(hhdm, boot.memory_map);
            if boot.memory_map_dropped > 0 {
                warn!("mem:  {} memory-map entries did not fit the boot buffer", boot.memory_map_dropped);
            }
            debug!("mem:  hhdm offset ...... {:#018x}", hhdm);
            let (n, unit) = human(hal::memory::usable_bytes());
            debug!(
                "mem:  usable ........... {} {} in {} regions",
                n,
                unit,
                hal::memory::region_count()
            );
            hal::memory::for_each_region(|r| {
                trace!(
                    "mem:    {:#018x}-{:#018x}  {}",
                    r.base,
                    r.end(),
                    r.kind.name()
                );
            });
        }
        _ => warn!("mem:  HHDM or memory map missing"),
    }

    // 5c. Physical memory manager: owns every usable frame from here on
    // (paging, below, takes its page tables from it). Needs only the HHDM.
    match mm::pmm::init() {
        Ok(i) => {
            let (m, mu) = human(i.managed_frames * mm::pmm::FRAME_SIZE);
            debug!(
                "pmm:  managed .......... {} {} in {} range(s), {} frames (usable, >= 1 MiB; {:#x}-{:#x})",
                m, mu, i.ranges, i.managed_frames, i.lowest, i.highest
            );
            debug!(
                "pmm:  bitmap ........... {} bytes = {} frame(s) at {:#x} (1 bit/frame, covers frames 0..{})",
                i.meta_bytes, i.meta_frames, i.meta_phys, i.covered_frames
            );
            let (f, fu) = human(i.free_frames * mm::pmm::FRAME_SIZE);
            debug!("pmm:  free ............. {} frames ({} {})", i.free_frames, f, fu);
            if i.ranges_dropped > 0 {
                warn!("pmm:  {} range(s) did not fit the table and are unmanaged", i.ranges_dropped);
            }
            match mm::pmm::self_test() {
                Ok(t) => {
                    debug!(
                        "pmm:  self-test ........ {} frames cycled, zero-fill reused {} dirty frame(s), {} bad frees rejected, 64 KiB-aligned run at {:#x}",
                        t.frames_cycled, t.dirty_reused, t.rejected, t.contiguous_base
                    );
                    match t.below_16m {
                        Some(a) => debug!("pmm:  address limit .... a run below 16 MiB was allocated at {:#x}", a),
                        None => debug!("pmm:  address limit .... no usable memory below 16 MiB; limit checked with an impossible one"),
                    }
                }
                Err(why) => error!("pmm:  self-test FAILED: {}", why),
            }
        }
        Err(e) => error!("pmm:  initialisation FAILED: {:?}", e),
    }

    // 6. Boot CPU: the architecture sets up its descriptor tables, per-CPU data and interrupts.
    let bsp_hw_id = arch::bringup::init_boot_cpu(boot);

    // 7. MMU self-tests.
    arch::bringup::mmu_self_test();

    // 7a. Virtual memory manager (regions, guard pages, W^X, address spaces),
    // then the kernel heap on top of it. From here on `alloc` works.
    match mm::vmm::init() {
        Ok(i) => {
            let (sz, su) = human(i.arena_size);
            debug!(
                "vmm:  arena ............ {:#018x} + {} {} (one PML4 slot), NX {}, kernel root {:#x}",
                i.arena_base,
                sz,
                su,
                if i.nx { "enforced" } else { "NOT available" },
                i.kernel_root
            );
            debug!("vmm:  shared PML4 slots . {} created so address spaces can share them", i.slots_created);
            match mm::vmm::self_test() {
                Ok(t) => debug!(
                    "vmm:  self-test ......... {} allocs, {} guard pages hold, {} bad requests rejected (W^X, user-in-kernel, overlap, ...), tables reclaimed: {}; 2 address spaces isolated, {} frames reclaimed",
                    t.allocs, t.guards_checked, t.rejected, t.tables_reclaimed, t.address_space_frames
                ),
                Err(why) => error!("vmm:  self-test FAILED: {}", why),
            }
        }
        Err(e) => error!("vmm:  initialisation FAILED: {:?}", e),
    }
    match mm::heap::init() {
        Ok(i) => {
            let (sz, su) = human(i.arena_pages * mm::vmm::PAGE_SIZE);
            debug!(
                "heap: arena ............ {:#018x}, {} {} reserved, committed on demand (8 size classes 16 B..{} B, larger -> VMM)",
                i.arena_base,
                sz,
                su,
                mm::heap::MAX_SMALL
            );
            match mm::heap::self_test() {
                Ok(t) => debug!(
                    "heap: self-test ........ Box/Vec/String/BTreeMap ok (largest Vec {} KiB), {} stress ops, {} invalid frees rejected",
                    t.largest_vec / 1024,
                    t.stress_ops,
                    t.bad_frees_rejected
                ),
                Err(why) => error!("heap: self-test FAILED: {}", why),
            }
        }
        Err(e) => error!("heap: initialisation FAILED: {:?}", e),
    }

    // 7a'. Object Manager: "everything is an object". Types, the namespace
    // (`\`), the kernel handle table.
    match ob::init() {
        Ok(i) => {
            debug!(
                "ob:   object manager ... {} types, root `\\` + {} directories, kernel handle table ready",
                i.types, i.directories
            );
            match ob::self_test() {
                Ok(t) => debug!(
                    "ob:   self-test ......... {} checks (names, rights, counts, symlinks, 1000-handle table, hooks, address-space object), {} stress ops, nothing leaked",
                    t.checks, t.stress_ops
                ),
                Err(why) => error!("ob:   self-test FAILED: {}", why),
            }
        }
        Err(e) => error!("ob:   initialisation FAILED: {:?}", e),
    }

    // 7a''. IPC Manager: ports (message queues, request/reply, handle passing) and
    // sections (shared memory), both Object Manager types.
    match ipc::init() {
        Ok(i) => debug!(
            "ipc:  ipc manager ....... {} types (Port, Section), {} directories (\\Ports, \\Sections), syscalls {}-{}",
            i.types,
            i.directories,
            ipc::syscall::SYS_PORT_CREATE,
            ipc::syscall::SYS_HANDLE_DUP
        ),
        Err(e) => error!("ipc:  initialisation FAILED: {:?}", e),
    }

    // 7b. Interrupt controller of the boot CPU, then time (reference clock + per-CPU timer).
    arch::bringup::init_interrupt_controller();
    arch::bringup::init_timers(boot);

    // 8. Other CPUs (hardware threads / cores).
    arch::bringup::start_secondary_cpus(boot, bsp_hw_id);

    register_kernel_objects(boot, bsp_hw_id);

    let s = mm::pmm::stats();
    let (fm, fu) = human(s.free_bytes());
    debug!(
        "pmm:  after boot ....... {} frame(s) in use (peak {}), {} free ({} {}), {} bad free(s) rejected",
        s.in_use_frames, s.peak_in_use_frames, s.free_frames, fm, fu, s.free_rejected
    );
    match mm::pmm::verify() {
        Ok(()) => debug!("pmm:  bitmap check ..... all invariants hold"),
        Err(why) => error!("pmm:  bitmap check FAILED: {}", why),
    }
    let v = mm::vmm::stats();
    debug!(
        "vmm:  after boot ....... {} region(s), {} page(s) reserved, {} committed (peak {})",
        v.regions, v.reserved_pages, v.committed_pages, v.peak_committed_pages
    );
    match mm::vmm::verify() {
        Ok(()) => debug!("vmm:  region check ..... table sorted, counters match the page tables, guards unmapped"),
        Err(why) => error!("vmm:  region check FAILED: {}", why),
    }
    mm::vmm::for_each_region(|r| {
        trace!(
            "vmm:    {:#018x} {:>6} pg ({} committed) {:<8} {}{}{}",
            r.start,
            r.pages,
            r.committed,
            r.kind.name(),
            r.tag,
            if r.guard_lo { " [guard-lo]" } else { "" },
            if r.guard_hi { " [guard-hi]" } else { "" }
        );
    });
    let h = mm::heap::stats();
    debug!(
        "heap: after boot ....... {} B live (peak {} B), {} allocs / {} frees, {} arena page(s) committed, {} failed, {} bad free(s), {} double free(s)",
        h.live_bytes,
        h.peak_live_bytes,
        h.allocs,
        h.frees,
        h.arena_committed_pages,
        h.failed,
        h.bad_frees,
        h.double_frees
    );
    {
        let o = ob::stats();
        debug!(
            "ob:   after boot ........ {} object(s) of {} type(s) (peak {}), {} open handle(s)",
            o.objects, o.types, o.peak_objects, o.handles
        );
        for t in ob::type_stats() {
            trace!("ob:     {:<13} {:>3} object(s), {} handle(s)", t.name, t.objects, t.handles);
        }
        for e in ob::namespace::snapshot() {
            let indent = "  ".repeat(e.depth);
            match &e.link_target {
                Some(t) => debug!("ob:   \\ {}{} [{}] -> {}", indent, e.name, e.type_name, t),
                None => debug!("ob:   \\ {}{} [{}]{}", indent, e.name, e.type_name, if e.permanent { " permanent" } else { "" }),
            }
        }
    }
    match mm::heap::verify() {
        Ok(()) => debug!("heap: free-list check .. canaries intact, carved == free + live for every class"),
        Err(why) => error!("heap: free-list check FAILED: {}", why),
    }

    // 9. Process manager: processes and threads as objects, then the scheduler.
    match ps::init() {
        Ok(()) => debug!(
            "ps:   process manager ... System process (pid 0) + idle thread (tid 0) created; {} priority levels, {} Hz tick, {}-tick quantum",
            ps::structs::priority::LEVELS,
            ps::TICK_HZ,
            ps::QUANTUM_TICKS
        ),
        Err(e) => {
            error!("ps:   initialisation FAILED: {:?}", e);
            info!("Initialisation complete (without a scheduler). Halting CPU.");
            arch::cpu::halt_forever()
        }
    }
    info!("Starting the scheduler: the boot CPU becomes the idle thread, the rest of the boot runs in a thread");
    if let Err(e) = ps::start(boot_driver) {
        error!("ps:   cannot start the scheduler: {:?}", e);
        arch::cpu::halt_forever()
    }
    ps::idle_loop()
}

/// The first real thread: runs the process manager's self-test under the live
/// scheduler, reports, and finishes the boot.
fn boot_driver(_: usize) {
    match ps::run_self_test() {
        Ok(t) => debug!(
            "ps:   self-test ......... 2 pinned spinners shared one CPU ({} and {} ticks, {} context switches), wake latency {} tick(s), sleep(50 ms) = {} ticks, \
             events/timeouts/terminate ok, {} user processes ({} syscalls in the first), {} short threads, nothing leaked",
            t.fair_ticks.0, t.fair_ticks.1, t.fair_switches, t.wake_latency_ticks, t.sleep_ticks, t.user_processes, t.user_syscalls, t.stress_threads
        ),
        Err(why) => error!("ps:   self-test FAILED: {}", why),
    }
    match ps::run_smp_self_test() {
        Ok(t) => debug!(
            "ps:   smp self-test ...... {} CPU(s): busy threads ran on {} of them at once, {} pinned threads stayed put, {} running threads moved by affinity, \
             {} threads stolen by idle CPUs, {} remote kills, {} cross-CPU ping-pongs, {} TLB rounds from ring 3 ({} shootdowns, {} TLB IPIs), \
             {} kernel map/unmap ops from all CPUs, {} reschedule IPIs, nothing leaked",
            t.cpus, t.busy_cpus, t.pinned_checks, t.moves, t.stolen, t.remote_kills, t.pingpong_rounds, t.tlb_rounds, t.shootdowns, t.tlb_ipis,
            t.map_ops, t.resched_ipis
        ),
        Err(why) => error!("ps:   smp self-test FAILED: {}", why),
    }
    match ipc::self_test() {
        Ok(t) => debug!(
            "ipc:  self-test ......... {} groups: {} messages ({} bytes) through ports, {} request/reply round trips, handles passed in messages, \
             {} shared-memory views (kernel and ring 3), {} IPC syscalls from ring 3, a blocked process killed cleanly, {} messages in the stress run, nothing leaked",
            t.checks, t.messages, t.bytes, t.requests, t.views, t.user_syscalls, t.stress_messages
        ),
        Err(why) => error!("ipc:  self-test FAILED: {}", why),
    }
    if let Err(e) = ld::init() {
        error!("ld:   cannot register the built-in libraries: {}", e.reason());
    }
    #[cfg(target_arch = "x86_64")]
    match ld::selftest::run() {
        Ok(t) => debug!(
            "ld:   self-test ......... LXF loader: {} libraries registered, program with relocation + imports + LibraryMain ran (exit code {}), {} malformed images refused, nothing leaked",
            ld::library_count(), t.exit_code, t.refused
        ),
        Err(why) => error!("ld:   self-test FAILED: {}", why),
    }
    #[cfg(target_arch = "x86_64")]
    match ex::selftest::run() {
        Ok(t) => debug!(
            "ex:   self-test ......... `syscall`/`sysret` gate, {} steps in ring 3 (time, sysinfo, vm_alloc/free/protect, events, wait, user threads, child processes, terminate, bad pointers), {} fast calls, nothing leaked",
            t.steps, t.fast_calls
        ),
        Err(why) => error!("ex:   self-test FAILED: {}", why),
    }
    let is = ipc::stats();
    debug!(
        "ipc:  after boot ........ {} port(s), {} section(s) alive; {} messages sent, {} received, {} requests, {} replies, {} view(s) mapped, {} syscalls",
        is.ports, is.sections, is.sent, is.received, is.requests, is.replies, is.views_mapped, is.syscalls
    );
    let st = ps::stats();
    debug!(
        "ps:   after boot ....... {} ticks, {} context switches ({} by preemption, {} thread migrations), {} process(es), {} thread(s), {} ready, {} CPU(s) scheduling",
        st.ticks, st.switches, st.preemptions, st.migrations, st.processes, st.threads, st.ready, st.cpus
    );
    for c in 0..hal::MAX_CPUS {
        let cs = ps::cpu_stats(c);
        if cs.active {
            debug!(
                "ps:     cpu {} ........ {} switches, {} ticks ({} idle), {} stolen, {} resched IPIs sent / {} received, {} TLB flushes served, {} queued",
                c, cs.switches, cs.ticks, cs.idle_ticks, cs.stolen, cs.ipis_sent, cs.ipis_received, arch::tlb::served_by(c), cs.ready
            );
        }
    }
    let tl = arch::tlb::stats();
    debug!(
        "tlb:  after boot ....... {} shootdown(s) reached other CPUs ({} IPIs), {} satisfied locally",
        tl.shootdowns, tl.ipis, tl.local_only
    );
    for p in ps::process_infos() {
        debug!(
            "ps:     pid {:>3} (parent {}) {:<8} {} thread(s) of {} ever, prio {}, {}, since tick {}{}",
            p.pid,
            p.parent,
            p.name,
            p.threads,
            p.total_threads,
            p.base_priority,
            if p.exited { "exited" } else { "active" },
            p.created_tick,
            if p.user { ", user mode" } else if p.exited && p.exit_code != 0 { ", failed" } else { "" }
        );
    }
    for t in ps::thread_infos() {
        debug!(
            "ps:     tid {:>3} pid {:>3} {:<12} {:<10} prio {:>2} (base {:>2}) cpu {} (mask {:#x}, {} moves), {} cpu ticks, {} switches in, created at tick {}{}",
            t.tid,
            t.pid,
            t.name,
            t.state.name(),
            t.priority,
            t.base_priority,
            t.cpu,
            t.affinity,
            t.migrations,
            t.cpu_ticks,
            t.switches_in,
            t.created_tick,
            if t.user { ", ring 3" } else { "" }
        );
    }
    info!("Initialisation complete. The system is running.");
}

#[panic_handler]
fn panic(panic_info: &PanicInfo) -> ! {
    // Stop the other CPUs first (NMI): they must not keep running on a broken kernel, and they
    // may hold the log locks. If another CPU is already panicking, leave the report to it.
    if !arch::smp::stop_other_cpus() {
        arch::cpu::halt_forever();
    }
    log::unlock_for_panic();
    error!("*** KERNEL PANIC ***");
    // `PanicInfo`'s Display impl prints the location and the message.
    error!("{}", panic_info);
    arch::cpu::halt_forever()
}
