// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Memory governor policy tests: shared DAX cache floor sizing, Linux PSI parsing, sliding refault
//! window, soft-dip idle-only protection, rate-limited cold mapped shrink, refault thrash backoff,
//! and own-cgroup OOM guard.

#![cfg(any(target_os = "linux", madsim))]

use cladefs::cladefsd::governor::{
    HostMemory, INTERVAL, MAPPED_SHRINK_MAX, MAPPED_SHRINK_MIN, MemoryPressure, MemorySample,
    PolicyState, REFAULT_THRASH_ENTER, REFAULT_THRASH_EXIT, ReclaimMode, RefaultWindow,
    VM_RECLAIM_MIN_SWAP_FREE, VM_RECLAIM_PASS_MAX, VM_RECLAIM_PSI_LIMIT_BPS, VM_RECLAIM_STEP_MAX,
    VM_RECLAIM_STEP_MIN, VmCgroupStats, allowance, cgroup_v2_path_of, decide, floor_of,
    has_cap_dac_override, headroom_of, mglru_enabled_of, psi_of, swap_free_of, swap_total_of,
    vm_reclaim_pass_budget, vm_reclaim_swap_reserve, vm_reclaim_target,
};
use cladefs::slabs::SLAB;
use std::time::Duration;

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

#[test]
fn default_floor_scales_as_one_eighth_of_limit_and_honors_override() {
    // 27.7 GiB host (ubu1): ~3.46 GiB shared DAX working-set floor, not 443 MiB.
    let ubu1 = 29_742_647_296u64;
    let ubu1_floor = floor_of(ubu1, None).unwrap();
    assert_eq!(ubu1_floor % SLAB, 0);
    assert_eq!(ubu1_floor, (ubu1 / 8) / SLAB * SLAB);
    assert!(ubu1_floor > 3 * GIB, "expected >3 GiB floor on 27.7 GiB host, got {ubu1_floor}");

    assert_eq!(floor_of(64 * GIB, None), Ok(8 * GIB));
    assert_eq!(floor_of(17 * GIB, None), Ok(2 * GIB + 128 * MIB));
    assert_eq!(floor_of(8 * GIB, None), Ok(GIB));
    assert_eq!(floor_of(2 * GIB, None), Ok(256 * MIB));
    assert_eq!(floor_of(GIB, None), Ok(128 * MIB));
    assert_eq!(floor_of(512 * MIB, None), Ok(64 * MIB));
    // Tiny limit clamps up to one slab.
    assert_eq!(floor_of(SLAB, None), Ok(SLAB));
    assert_eq!(floor_of(0, None), Ok(SLAB));

    // Explicit override rounds down to whole slabs (min 1 slab) and rejects malformed text.
    assert_eq!(floor_of(ubu1, Some(&(512 * MIB).to_string())), Ok(512 * MIB));
    assert_eq!(floor_of(ubu1, Some("1")), Ok(SLAB));
    assert!(floor_of(ubu1, Some("4G")).is_err());
}

#[test]
fn psi_parser_handles_valid_and_malformed_inputs() {
    let sample = "some avg10=12.34 avg60=5.67 avg300=0.50 total=987654\n\
                  full avg10=1.05 avg60=0.00 avg300=0.00 total=12345\n";
    assert_eq!(
        psi_of(sample),
        Some(MemoryPressure {
            some_avg10_bps: 1234,
            full_avg10_bps: 105,
            some_total_us: 987_654,
            full_total_us: 12_345,
        })
    );
    // Edge percentages: 0, 100.00, single fractional digit, integer without dot, missing full.
    let edge = "some avg10=100.00 avg60=0.00 avg300=0.00 total=0\n";
    assert_eq!(
        psi_of(edge),
        Some(MemoryPressure {
            some_avg10_bps: 10_000,
            full_avg10_bps: 0,
            some_total_us: 0,
            full_total_us: 0,
        })
    );
    let one_digit = "some avg10=0.5 avg60=0 total=42\nfull avg10=2 avg60=0 total=7\n";
    assert_eq!(
        psi_of(one_digit),
        Some(MemoryPressure {
            some_avg10_bps: 50,
            full_avg10_bps: 200,
            some_total_us: 42,
            full_total_us: 7,
        })
    );
    // Reject out-of-range, negative, non-numeric, or missing required fields.
    for bad in [
        "",
        "full avg10=1.00 total=10\n",
        "some avg10=100.01 total=10\n",
        "some avg10=-1.00 total=10\n",
        "some avg10=NaN total=10\n",
        "some avg10=1.00\n",
        "some avg10=999999999999999999999 total=10\n",
    ] {
        assert_eq!(psi_of(bad), None, "expected None for {bad:?}");
    }
}

#[test]
fn refault_window_sums_last_second_normalizes_slow_ticks_and_saturates_on_reset() {
    let mut window = RefaultWindow::new(100);
    // 10 ticks of 5 refaults each at 100 ms -> 50 refaults/sec.
    for i in 1..=10 {
        assert_eq!(window.record(100 + i * 5), i * 5);
    }
    // 11th tick with 0 new refaults drops the oldest tick's 5.
    assert_eq!(window.record(150), 45);
    // Counter going backwards saturates delta to 0 without panicking.
    assert_eq!(window.record(10), 40);

    // Slow 400 ms ticks with 10 refaults per tick = 25 refaults/sec (never miscounted as 100/s).
    let mut slow = RefaultWindow::new(0);
    let mut total = 0;
    for _ in 0..5 {
        total += 10;
        let rate = slow.record_elapsed(total, Duration::from_millis(400));
        assert!(rate <= 25, "expected <= 25/s on 400ms ticks, got {rate}");
    }
    assert_eq!(slow.record_elapsed(total + 10, Duration::from_millis(400)), 25);

    // Single stalled 2.5 s tick with 50 refaults -> normalized to 20/s.
    let mut stalled = RefaultWindow::new(0);
    assert_eq!(stalled.record_elapsed(50, Duration::from_millis(2500)), 20);
}

#[test]
fn soft_memavailable_dip_and_sibling_slice_psi_do_not_evict_mapped_extents() {
    let total = 29_742_647_296u64; // ubu1: 27.7 GiB
    let headroom = headroom_of(total, None).unwrap(); // ~1.73 GiB
    let floor = floor_of(total, None).unwrap(); // ~3.46 GiB
    let mut state = PolicyState::default();

    // Sibling VM cgroups hit their own memory.high: host PSI spikes to 35%, MemAvailable dips to
    // 1.2 GiB (< 1.73 GiB headroom, >= 865 MiB critical), while cladefsd's own cgroup PSI is 0%.
    // Must shed idle extents only and hold mapped working set.
    let sample = MemorySample {
        host: HostMemory { total, available: 1200 * MIB },
        host_available: 1200 * MIB,
        cgroup_available: None,
        host_psi: Some(MemoryPressure {
            some_avg10_bps: 3500,
            full_avg10_bps: 800,
            some_total_us: 5_000_000,
            full_total_us: 1_000_000,
        }),
        cgroup_psi: Some(MemoryPressure::default()),
        host_swap_free: 8 * GIB,
        host_swap_total: 14 * GIB,
    };
    let decision = decide(6 * GIB, sample, headroom, floor, 0, INTERVAL, &mut state);
    assert_eq!(decision.mode, ReclaimMode::IdleOnly);
    assert!(decision.pressured, "must still signal pressure to VM orchestrators");
    assert!(!decision.full);
    assert!(!decision.thrashing);
    assert!(decision.allowance >= floor);
}

#[test]
fn genuine_host_pressure_shrinks_cold_mapped_extents_and_backs_off_on_refault_thrashing() {
    let total = 29_742_647_296u64;
    let headroom = headroom_of(total, None).unwrap(); // ~1.73 GiB (critical ~865 MiB, emergency ~432 MiB)
    let floor = floor_of(total, None).unwrap(); // ~3.46 GiB
    let mut state = PolicyState::default();

    // Tick 1: MemAvailable drops to 600 MiB (< critical, >= emergency) with active host PSI (15%),
    // 0 refaults. Cold mapped extents should shrink gradually (rate-limited).
    let pressured_sample = MemorySample {
        host: HostMemory { total, available: 600 * MIB },
        host_available: 600 * MIB,
        cgroup_available: None,
        host_psi: Some(MemoryPressure {
            some_avg10_bps: 1500,
            full_avg10_bps: 100,
            some_total_us: 1_000_000,
            full_total_us: 50_000,
        }),
        cgroup_psi: Some(MemoryPressure::default()),
        host_swap_free: 0,
        host_swap_total: 0,
    };
    let cold = decide(10 * GIB, pressured_sample, headroom, floor, 0, INTERVAL, &mut state);
    match cold.mode {
        ReclaimMode::Mapped { max_shrink_bytes } => {
            assert!(
                (MAPPED_SHRINK_MIN..=MAPPED_SHRINK_MAX).contains(&max_shrink_bytes),
                "expected rate-limited shrink step, got {max_shrink_bytes}"
            );
        }
        other => panic!("expected rate-limited Mapped shrink for cold cache, got {other:?}"),
    }
    assert!(cold.pressured);
    assert!(!cold.thrashing);

    // Tick 2: Evicting those extents causes refaults >= REFAULT_THRASH_ENTER (32/s):
    // governor must enter thrashing and back off to IdleOnly!
    let thrash = decide(
        10 * GIB,
        pressured_sample,
        headroom,
        floor,
        REFAULT_THRASH_ENTER,
        INTERVAL,
        &mut state,
    );
    assert_eq!(thrash.mode, ReclaimMode::IdleOnly);
    assert!(thrash.thrashing);
    assert!(thrash.pressured);

    // Tick 2b: If host MemAvailable drops below emergency (headroom / 4 = ~432 MiB) even while
    // thrashing, thrashing MUST NOT veto host give-back (prevents host OOM killing VMs).
    let emergency_sample = MemorySample {
        host: HostMemory { total, available: 300 * MIB },
        host_available: 300 * MIB,
        ..pressured_sample
    };
    let emergency = decide(
        10 * GIB,
        emergency_sample,
        headroom,
        floor,
        REFAULT_THRASH_ENTER * 4,
        INTERVAL,
        &mut state,
    );
    assert!(emergency.thrashing);
    assert!(
        matches!(
            emergency.mode,
            ReclaimMode::Mapped { max_shrink_bytes }
                if (MAPPED_SHRINK_MIN..=MAPPED_SHRINK_MAX).contains(&max_shrink_bytes)
        ),
        "emergency host watermark must override thrashing with rate-limited shrink, got {:?}",
        emergency.mode
    );

    // Tick 3: Back above emergency, refaults drop to 12/s (> REFAULT_THRASH_EXIT = 8): hysteresis
    // keeps thrashing and IdleOnly.
    let hyst = decide(10 * GIB, pressured_sample, headroom, floor, 12, INTERVAL, &mut state);
    assert_eq!(hyst.mode, ReclaimMode::IdleOnly);
    assert!(hyst.thrashing);

    // Tick 4: Refaults drop to REFAULT_THRASH_EXIT (8/s): thrashing clears, mapped shrink resumes.
    let recovered = decide(
        10 * GIB,
        pressured_sample,
        headroom,
        floor,
        REFAULT_THRASH_EXIT,
        INTERVAL,
        &mut state,
    );
    assert!(!recovered.thrashing);
    assert!(matches!(recovered.mode, ReclaimMode::Mapped { .. }));
}

#[test]
fn own_cgroup_limit_and_own_psi_govern_reclaim_correctly() {
    let host_total = 64 * GIB;
    let cgroup_limit = 8 * GIB;
    let headroom = headroom_of(cgroup_limit, None).unwrap(); // 1 GiB (emergency = 256 MiB)
    let floor = floor_of(cgroup_limit, None).unwrap(); // 1 GiB
    let mut state = PolicyState::default();

    // First tick with large historical cgroup PSI `total` (avg10=0) must NOT falsely trigger
    // instantaneous delta pressure on daemon boot.
    let boot_sample = MemorySample {
        host: HostMemory { total: host_total, available: 512 * MIB },
        host_available: 512 * MIB,
        cgroup_available: None,
        host_psi: Some(MemoryPressure {
            some_avg10_bps: 0,
            full_avg10_bps: 0,
            some_total_us: 999_999_999,
            full_total_us: 0,
        }),
        cgroup_psi: Some(MemoryPressure {
            some_avg10_bps: 0,
            full_avg10_bps: 0,
            some_total_us: 888_888_888,
            full_total_us: 0,
        }),
        host_swap_free: 0,
        host_swap_total: 0,
    };
    let boot = decide(4 * GIB, boot_sample, headroom, floor, 0, INTERVAL, &mut state);
    assert_eq!(boot.mode, ReclaimMode::IdleOnly, "boot tick must not spike on historical PSI");

    // Own cgroup tight (headroom/4 <= cgroup_available < headroom) while the host itself has
    // ample MemAvailable (32 GiB): rate-limited Mapped shrink when not thrashing, and raw
    // host_available keeps proactive VM reclaim budget at 0 (never swaps VMs for own cgroup cap).
    let soft_tight_cgroup = MemorySample {
        host: HostMemory { total: host_total, available: 512 * MIB },
        host_available: 32 * GIB,
        cgroup_available: Some(512 * MIB),
        host_psi: Some(MemoryPressure::default()),
        cgroup_psi: Some(MemoryPressure::default()),
        host_swap_free: 8 * GIB,
        host_swap_total: 16 * GIB,
    };
    assert_eq!(vm_reclaim_pass_budget(headroom, soft_tight_cgroup.host_available), 0);
    let soft_tight = decide(4 * GIB, soft_tight_cgroup, headroom, floor, 0, INTERVAL, &mut state);
    assert!(
        matches!(
            soft_tight.mode,
            ReclaimMode::Mapped { max_shrink_bytes }
                if (MAPPED_SHRINK_MIN..=MAPPED_SHRINK_MAX).contains(&max_shrink_bytes)
        ),
        "soft cgroup tight must be rate-limited, got {:?}",
        soft_tight.mode
    );

    // Own cgroup emergency (`cgroup_available < headroom / 4`): MUST shrink promptly (`u64::MAX`)
    // even while refaults are high, to avoid cgroup OOM kill.
    let emergency_cgroup = MemorySample {
        host: HostMemory { total: host_total, available: 200 * MIB },
        host_available: 32 * GIB,
        cgroup_available: Some(200 * MIB),
        host_psi: Some(MemoryPressure::default()),
        cgroup_psi: Some(MemoryPressure::default()),
        host_swap_free: 0,
        host_swap_total: 0,
    };
    let oom_guard = decide(
        4 * GIB,
        emergency_cgroup,
        headroom,
        floor,
        REFAULT_THRASH_ENTER * 10,
        INTERVAL,
        &mut state,
    );
    assert_eq!(oom_guard.mode, ReclaimMode::Mapped { max_shrink_bytes: u64::MAX });
    assert!(oom_guard.thrashing);
    assert!(oom_guard.pressured);

    // PSI delta normalization: 20 ms stall across a 400 ms tick is only 5% (< 10% threshold),
    // so it must NOT trigger own-PSI pressure; 50 ms across 100 ms (50% >= 10%) MUST trigger.
    state.thrashing = false;
    state.prev_cgroup_some_us = Some(0);
    let slow_small_stall = MemorySample {
        host: HostMemory { total: host_total, available: 700 * MIB },
        host_available: 700 * MIB,
        cgroup_available: None,
        host_psi: Some(MemoryPressure::default()),
        cgroup_psi: Some(MemoryPressure {
            some_avg10_bps: 0,
            full_avg10_bps: 0,
            some_total_us: 20_000,
            full_total_us: 0,
        }),
        host_swap_free: 0,
        host_swap_total: 0,
    };
    let slow = decide(
        4 * GIB,
        slow_small_stall,
        headroom,
        floor,
        0,
        Duration::from_millis(400),
        &mut state,
    );
    assert_eq!(slow.mode, ReclaimMode::IdleOnly, "5% stall over 400ms must not trigger own PSI");

    let own_spike = MemorySample {
        cgroup_psi: Some(MemoryPressure {
            some_avg10_bps: 0,
            full_avg10_bps: 0,
            some_total_us: 70_000,
            full_total_us: 0,
        }),
        ..slow_small_stall
    };
    let spiked = decide(4 * GIB, own_spike, headroom, floor, 0, INTERVAL, &mut state);
    assert!(matches!(
        spiked.mode,
        ReclaimMode::Mapped { max_shrink_bytes } if max_shrink_bytes < u64::MAX
    ));

    // When room drops under the 1 GiB cgroup floor, `full` triggers and `allowance` clamps to floor.
    let squeezed =
        allowance(0, HostMemory { total: host_total, available: headroom }, headroom, floor, false);
    assert!(squeezed.full);
    assert_eq!(squeezed.bytes, floor);
}

#[test]
fn tmo_vm_reclaim_policy_reclaims_only_cold_vms_with_swap_headroom() {
    let swap_total = 16 * GIB; // reserve = 2 GiB (1/8)
    assert_eq!(vm_reclaim_swap_reserve(swap_total), 2 * GIB);
    assert_eq!(vm_reclaim_swap_reserve(GIB), VM_RECLAIM_MIN_SWAP_FREE);

    // Pass budget is bounded by headroom deficit and VM_RECLAIM_PASS_MAX.
    assert_eq!(vm_reclaim_pass_budget(GIB, GIB), 0);
    assert_eq!(vm_reclaim_pass_budget(GIB, GIB - 2 * MIB), 0);
    assert_eq!(vm_reclaim_pass_budget(GIB, GIB - 20 * MIB), 20 * MIB);
    assert_eq!(vm_reclaim_pass_budget(2 * GIB, 0), VM_RECLAIM_PASS_MAX);

    let cold_vm = VmCgroupStats {
        current_bytes: 4 * GIB,
        swap_current_bytes: 512 * MIB,
        swap_max_bytes: Some(4 * GIB),
        psi: Some(MemoryPressure {
            some_avg10_bps: 25,
            full_avg10_bps: 0,
            some_total_us: 0,
            full_total_us: 0,
        }),
    };
    assert_eq!(
        vm_reclaim_target(cold_vm, 8 * GIB, swap_total, VM_RECLAIM_STEP_MAX),
        VM_RECLAIM_STEP_MAX
    );

    // Missing PSI must never be proactively reclaimed (blind reclaim).
    let no_psi = VmCgroupStats { psi: None, ..cold_vm };
    assert_eq!(vm_reclaim_target(no_psi, 8 * GIB, swap_total, VM_RECLAIM_STEP_MAX), 0);

    // Active VM with PSI some >= 1% or any full stall must be left alone.
    let hot_some = VmCgroupStats {
        psi: Some(MemoryPressure {
            some_avg10_bps: VM_RECLAIM_PSI_LIMIT_BPS,
            ..Default::default()
        }),
        ..cold_vm
    };
    assert_eq!(vm_reclaim_target(hot_some, 8 * GIB, swap_total, VM_RECLAIM_STEP_MAX), 0);
    let hot_full = VmCgroupStats {
        psi: Some(MemoryPressure { full_avg10_bps: 1, ..Default::default() }),
        ..cold_vm
    };
    assert_eq!(vm_reclaim_target(hot_full, 8 * GIB, swap_total, VM_RECLAIM_STEP_MAX), 0);

    // Host swap at or below reserve (1/8 of SwapTotal = 2 GiB) -> no proactive reclaim.
    assert_eq!(vm_reclaim_target(cold_vm, 2 * GIB, swap_total, VM_RECLAIM_STEP_MAX), 0);
    assert_eq!(vm_reclaim_target(cold_vm, VM_RECLAIM_MIN_SWAP_FREE, GIB, VM_RECLAIM_STEP_MAX), 0);
    // VM cgroup with swap disabled or full -> no proactive reclaim.
    let no_swap_vm = VmCgroupStats { swap_max_bytes: Some(0), ..cold_vm };
    assert_eq!(vm_reclaim_target(no_swap_vm, 8 * GIB, swap_total, VM_RECLAIM_STEP_MAX), 0);
    let full_swap_vm =
        VmCgroupStats { swap_current_bytes: 4 * GIB, swap_max_bytes: Some(4 * GIB), ..cold_vm };
    assert_eq!(vm_reclaim_target(full_swap_vm, 8 * GIB, swap_total, VM_RECLAIM_STEP_MAX), 0);

    // Target caps at current_bytes / 8 and cgroup swap headroom, skipping sub-threshold slivers.
    let small_vm = VmCgroupStats { current_bytes: 200 * MIB, swap_max_bytes: None, ..cold_vm };
    assert_eq!(vm_reclaim_target(small_vm, 8 * GIB, swap_total, VM_RECLAIM_STEP_MAX), 25 * MIB);
    let tiny_vm = VmCgroupStats { current_bytes: 20 * MIB, swap_max_bytes: None, ..cold_vm };
    assert_eq!(vm_reclaim_target(tiny_vm, 8 * GIB, swap_total, VM_RECLAIM_STEP_MAX), 0);
    let tight_swap_vm = VmCgroupStats {
        swap_current_bytes: 4 * GIB - 10 * MIB,
        swap_max_bytes: Some(4 * GIB),
        ..cold_vm
    };
    assert_eq!(
        vm_reclaim_target(tight_swap_vm, 8 * GIB, swap_total, VM_RECLAIM_STEP_MAX),
        10 * MIB
    );
    assert!(10 * MIB >= VM_RECLAIM_STEP_MIN);
}

#[test]
fn swap_free_and_cgroup_v2_path_parsers_reject_malformed_inputs() {
    let meminfo =
        "MemTotal:       29045552 kB\nSwapTotal:      14680060 kB\nSwapFree:       10485760 kB\n";
    assert_eq!(swap_free_of(meminfo), 10 * GIB);
    assert_eq!(swap_total_of(meminfo), 14_680_060 * 1024);
    assert_eq!(swap_free_of("MemTotal: 1000 kB\n"), 0);
    assert_eq!(swap_total_of("MemTotal: 1000 kB\n"), 0);
    assert_eq!(swap_free_of("SwapFree: nope kB\n"), 0);

    assert_eq!(
        cgroup_v2_path_of("0::/system.slice/vm-123.service\n"),
        Some("system.slice/vm-123.service")
    );
    assert_eq!(cgroup_v2_path_of("0::/\n"), None);
    assert_eq!(cgroup_v2_path_of("0::/foo/../bar\n"), None);
    assert_eq!(cgroup_v2_path_of("1:memory:/legacy\n"), None);

    assert!(mglru_enabled_of("0x0007\n"));
    assert!(mglru_enabled_of("1\n"));
    assert!(!mglru_enabled_of("0x0000\n"));
    assert!(!mglru_enabled_of("0\n"));
    assert!(!mglru_enabled_of("nope\n"));

    assert!(has_cap_dac_override("CapPrm:\t0000000000000000\nCapEff:\t0000000000000023\n"));
    assert!(has_cap_dac_override("CapEff:\t000001ffffffffff\n"));
    assert!(!has_cap_dac_override("CapEff:\t0000000000000021\n"));
    assert!(!has_cap_dac_override("CapEff:\t0000000000000000\n"));
    assert!(!has_cap_dac_override("CapPrm:\t0000000000000002\n"));
}
