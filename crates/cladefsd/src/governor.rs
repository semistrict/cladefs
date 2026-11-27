// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the daemon's memory as a page cache (TASK-358): as Linux's page cache, it has no size
//! of its own, but takes whatever of the host's memory is free, and gives it back as others need
//! it. Under DAX, the daemon's memory is a guest's only cache of its files, and cladefs is the only
//! thing on its hosts: memory left free is memory wasted.
//!
//! A thread looks at the host's memory every [`INTERVAL`]: what is available to the daemon, the
//! host's (`MemAvailable`) or, if less, its cgroups', each ancestor's limit less what it holds
//! but could reclaim. The daemon may hold what it holds and that, less a headroom left for VMs'
//! memory to grow into between looks and for the host ([`headroom`]). It sets the limit on every
//! tenant's memory together to that ([`super::memory::govern`]); held past it, the daemon evicts at
//! once, each tenant first down to its share of it by weight ([`shares`]), then any, least recently
//! used first. cladefs: what it holds counts the parts of segments its cells pack as they fill,
//! held until their framings, which it lets go of first, before any tenant's page: derived, their
//! framings read them back (`super::memory::held_parts`, TASK-400).
//!
//! cladefs: never less than a floor ([`floor`], board 5662). Under DAX the daemon's pages are its
//! guests' page cache: squeezed below what their windows use, every fault evicts another's page,
//! and at the last finds none, which killed the VMs faulting (cells-per-host-dax at 159 VMs, the
//! daemon's limit down to a few chunks). So the governor keeps the floor whatever the host leaves,
//! and says the host is full while the room for the daemon is under it ([`full`]): the daemon then
//! refuses to make cells (`CreateCell`, `ForkCell`), so that the VM manager places no VM more here,
//! and what more the guests already running take comes out of the host's own headroom. And a
//! page-in finding no page waits for one rather than fails (`crate::cache`, `slabs::room_may_come`).
//!
//! A daemon given its memory's size (`cladefsd SOCKET MEMORY_BYTES`) is not governed: its
//! memory is that, as a test's daemon's may be.
//!
//! cladefs: the host's disk, governed alike (backlog doc-22, TASK-539.9): each filesystem the
//! daemon keeps cells or its part cache on, a disk (`bucket_cell::host_disk`), is the daemon's but
//! for a headroom ([`disk_headroom`]), and no more than `CLADEFSD_DISK_BYTES` of all of them
//! together, if that is set. Its cells come first; the part cache takes what they leave, evicted at
//! once as they need it; then, its cells near the allowance ([`pressured`]), their uploaders are
//! urged, and past it their guests' writes refused. The policy is here, its look at the disks in
//! `super::disk`.

use crate::Daemon;
use crate::memory;
use cladefs_core::slabs::SLAB;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// How often the governor looks at the host's memory.
pub const INTERVAL: Duration = Duration::from_millis(100);

/// What the governor leaves free of the memory available to the daemon:
/// `CLADEFSD_HEADROOM` bytes, or a sixteenth of the memory the daemon may have, `limit`
/// ([`limit`]), 1 GiB at least and half of `limit` at most: room for VMs' memory to grow into
/// between looks, and for the host's own (board 8357).
pub fn headroom_of(limit: u64, set: Option<&str>) -> Result<u64, String> {
    match set {
        Some(bytes) if !bytes.is_empty() => {
            bytes.parse().map_err(|_| format!("CLADEFSD_HEADROOM is bytes, not {bytes:?}"))
        }
        _ => Ok((limit / 16).max(1 << 30).min(limit / 2)),
    }
}

/// What the governor leaves free of the memory available to the daemon, reading
/// `CLADEFSD_HEADROOM` ([`headroom_of`]).
pub fn headroom(limit: u64) -> Result<u64, String> {
    match node_local::env::var("CLADEFSD_HEADROOM") {
        Ok(bytes) => headroom_of(limit, Some(&bytes)),
        // Unset or non-UTF8: use the default headroom for `limit`.
        Err(_) => headroom_of(limit, None),
    }
}

/// cladefs: the least the governor leaves the daemon's memory, however little the host has left:
/// `CLADEFSD_MEMORY_FLOOR` bytes, or an eighth of the memory the daemon may have, `limit`
/// ([`limit`]), in whole slabs (one slab at least): a viable shared DAX cache working-set floor
/// for every VM on the host (e.g. ~3.5 GiB on a 28 GiB host, 8 GiB on a 64 GiB host; previously
/// `limit / 64` = 443 MiB on 28 GiB, which caused a DAX refault death spiral when sibling VMs
/// grew anonymous RAM). Of the cgroup's limit, not the host's, where that is less: a floor past
/// what the cgroup holds would have its OOM killer end what relief would have shrunk (board 5662,
/// board 8357).
pub fn floor_of(limit: u64, set: Option<&str>) -> Result<u64, String> {
    let bytes = match set {
        Some(bytes) if !bytes.is_empty() => {
            bytes.parse().map_err(|_| format!("CLADEFSD_MEMORY_FLOOR is bytes, not {bytes:?}"))?
        }
        _ => limit / 8,
    };
    Ok((bytes / SLAB * SLAB).max(SLAB))
}

/// cladefs: the least the governor leaves the daemon's memory, reading `CLADEFSD_MEMORY_FLOOR`
/// ([`floor_of`]).
pub fn floor(limit: u64) -> Result<u64, String> {
    match node_local::env::var("CLADEFSD_MEMORY_FLOOR") {
        Ok(bytes) => floor_of(limit, Some(&bytes)),
        // Unset or non-UTF8: use the default floor for `limit`.
        Err(_) => floor_of(limit, None),
    }
}

/// cladefs: the memory the daemon may have: the host's, `total`, or, if less, the least limit of
/// its cgroups (memory.max, its own and each ancestor's).
#[cfg(target_os = "linux")]
pub fn limit(total: u64) -> u64 {
    let Ok(cgroups) = std::fs::read_to_string("/proc/self/cgroup") else { return total };
    let Some(path) = cgroups.lines().find_map(|line| line.strip_prefix("0::")) else {
        return total;
    };
    let mut dir = std::path::Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
    let mut limit = total;
    loop {
        // Unread: a cgroup with no memory controller, which limits nothing.
        let max = std::fs::read_to_string(dir.join("memory.max")).ok();
        // max, unlimited, is no number: no limit here.
        if let Some(max) = max.and_then(|max| max.trim().parse::<u64>().ok()) {
            limit = limit.min(max);
        }
        if !dir.pop() || dir == std::path::Path::new("/sys/fs") {
            return limit;
        }
    }
}

/// cladefs: the memory the daemon may have: the host's (macOS has no cgroups).
#[cfg(target_os = "macos")]
pub fn limit(total: u64) -> u64 {
    total
}

/// Per-cgroup exponential backoff state for proactive VM `memory.reclaim` when a cgroup yields
/// zero reclaimed bytes or returns an error.
#[cfg(all(target_os = "linux", not(madsim)))]
struct VmReclaimBackoff {
    next_try: std::time::Instant,
    delay_secs: u64,
}

/// cladefs: the memory governor's state, owned by [`Daemon`].
#[derive(Default)]
pub(super) struct State {
    full: AtomicBool,
    pressured: AtomicBool,
    thrashing: AtomicBool,
    fulls: AtomicU64,
    floor: AtomicU64,
    target: AtomicU64,
    refaults_per_sec: AtomicU64,
    psi_some_avg10_bps: AtomicU64,
    psi_full_avg10_bps: AtomicU64,
    kernel_cold_harvested_bytes: AtomicU64,
    vm_reclaim_bytes: AtomicU64,
    vm_reclaim_attempts: AtomicU64,
    vm_reclaim_errors: AtomicU64,
    #[cfg(all(target_os = "linux", not(madsim)))]
    cold_harvest_busy: AtomicBool,
    #[cfg(all(target_os = "linux", not(madsim)))]
    vm_reclaim_busy: AtomicBool,
    #[cfg(all(target_os = "linux", not(madsim)))]
    vm_reclaim_denied_logged: AtomicBool,
    /// `0` = `swappiness=max`, `1` = `swappiness=200`, `2` = plain byte count.
    #[cfg(all(target_os = "linux", not(madsim)))]
    vm_reclaim_swappiness_mode: std::sync::atomic::AtomicU8,
    #[cfg(all(target_os = "linux", not(madsim)))]
    vm_reclaim_backoff: std::sync::Mutex<std::collections::HashMap<String, VmReclaimBackoff>>,
    #[cfg(all(target_os = "linux", not(madsim)))]
    vm_reclaim_hold_until: std::sync::Mutex<Option<std::time::Instant>>,
}

/// cladefs: whether `daemon`'s host is full: the room the governor last found for the daemon's
/// memory was under its floor ([`allowance`]), so that the daemon makes no cell more (board 5662).
pub(super) fn full_on(daemon: &Daemon) -> bool {
    daemon.governor.full.load(Ordering::Relaxed)
}

/// cladefs: whether the process's default daemon's host is full.
#[cfg(not(madsim))]
pub fn full() -> bool {
    full_on(Daemon::process())
}

/// cladefs: what the governor last found and did, for the daemon's metrics and `GetStats`
/// (board 8565): whether the host is full ([`full`]); whether the daemon is under memory pressure;
/// whether its extent cache is actively refault-thrashing; how often the host became full; the
/// floor it keeps and the raw headroom-adjusted target allowance (0 for a daemon not governed);
/// the current 1-second DAX part refault rate; and the last observed memory PSI averages in basis
/// points (hundredths of a percent).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Governed {
    /// Whether the host is full.
    pub full: bool,
    /// Whether the daemon is under memory pressure (holding past target, cgroup PSI pressured, or
    /// thrashing).
    pub pressured: bool,
    /// Whether the extent cache is actively refault-thrashing (recently evicted parts faulting
    /// back in rapidly).
    pub thrashing: bool,
    /// How often the host became full.
    pub fulls: u64,
    /// The floor the governor keeps the daemon's memory at, 0 for a daemon not governed.
    pub floor: u64,
    /// The governor's headroom-adjusted target allowance, 0 for a daemon not governed.
    pub target: u64,
    /// Current 1-second sliding-window DAX part refault rate (parts/sec).
    pub refaults_per_sec: u64,
    /// Last observed memory PSI `some avg10` in basis points (`0..=10_000`, 0 if unavailable).
    pub psi_some_avg10_bps: u64,
    /// Last observed memory PSI `full avg10` in basis points (`0..=10_000`, 0 if unavailable).
    pub psi_full_avg10_bps: u64,
    /// Total bytes of cold DAX extent parts harvested via `cachestat(2)` after MGLRU paged them
    /// out to swap.
    pub kernel_cold_harvested_bytes: u64,
    /// Total bytes requested for proactive reclaim across low-PSI connected VM cgroups.
    pub vm_reclaim_bytes: u64,
    /// Total proactive `memory.reclaim` writes issued against connected VM cgroups.
    pub vm_reclaim_attempts: u64,
    /// Total proactive `memory.reclaim` writes that failed with an error other than `EAGAIN`.
    pub vm_reclaim_errors: u64,
}

/// cladefs: what `daemon`'s governor last found and did ([`Governed`]).
pub(super) fn governed_on(daemon: &Daemon) -> Governed {
    Governed {
        full: full_on(daemon),
        pressured: daemon.governor.pressured.load(Ordering::Relaxed),
        thrashing: daemon.governor.thrashing.load(Ordering::Relaxed),
        fulls: daemon.governor.fulls.load(Ordering::Relaxed),
        floor: daemon.governor.floor.load(Ordering::Relaxed),
        target: daemon.governor.target.load(Ordering::Relaxed),
        refaults_per_sec: daemon.governor.refaults_per_sec.load(Ordering::Relaxed),
        psi_some_avg10_bps: daemon.governor.psi_some_avg10_bps.load(Ordering::Relaxed),
        psi_full_avg10_bps: daemon.governor.psi_full_avg10_bps.load(Ordering::Relaxed),
        kernel_cold_harvested_bytes: daemon
            .governor
            .kernel_cold_harvested_bytes
            .load(Ordering::Relaxed),
        vm_reclaim_bytes: daemon.governor.vm_reclaim_bytes.load(Ordering::Relaxed),
        vm_reclaim_attempts: daemon.governor.vm_reclaim_attempts.load(Ordering::Relaxed),
        vm_reclaim_errors: daemon.governor.vm_reclaim_errors.load(Ordering::Relaxed),
    }
}

/// cladefs: what the process's default daemon's governor last found and did ([`Governed`]).
#[cfg(not(madsim))]
pub fn governed() -> Governed {
    governed_on(Daemon::process())
}

/// cladefs: says the host is full, or not, on `daemon`, as the governor would: for tests of what
/// refuses then.
#[cfg(test)]
pub(crate) fn set_full_on(daemon: &Daemon, full: bool) {
    daemon.governor.full.store(full, Ordering::Relaxed);
}

/// The host's memory, as the daemon may use it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostMemory {
    /// All of it.
    pub total: u64,
    /// What is available to the daemon now: free, or reclaimable, the host's and its cgroups'.
    pub available: u64,
}

/// Linux Pressure Stall Information (PSI) for memory (`/proc/pressure/memory` or a cgroup's
/// `memory.pressure`): `some` and `full` 10-second averages in basis points (hundredths of a
/// percent, so `12.34%` is `1234`), and cumulative stall microseconds (`total=`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryPressure {
    /// `some avg10` in basis points (`0..=10_000`).
    pub some_avg10_bps: u32,
    /// `full avg10` in basis points (`0..=10_000`).
    pub full_avg10_bps: u32,
    /// Cumulative `some` stall microseconds (`total=`).
    pub some_total_us: u64,
    /// Cumulative `full` stall microseconds (`total=`).
    pub full_total_us: u64,
}

/// One governor observation of the host's and daemon's cgroup memory and PSI pressure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemorySample {
    /// Host total and effective available memory (`min(MemAvailable, cgroup_available)`).
    pub host: HostMemory,
    /// Raw host-wide `/proc/meminfo` `MemAvailable` in bytes (without clamping to `cladefsd`'s
    /// own cgroup `memory.max`). Proactive VM reclaim triggers only from raw host shortage, never
    /// because `cladefsd`'s own cgroup limit is tight.
    pub host_available: u64,
    /// Room remaining inside the daemon's own limited cgroup(s) (`memory.max - memory.current +
    /// inactive_file`), if any ancestor sets a finite `memory.max`. Tracked separately from
    /// host-wide `MemAvailable` because exhausting own-cgroup room triggers `cladefsd`'s own OOM
    /// killer regardless of sibling VMs.
    pub cgroup_available: Option<u64>,
    /// Host-wide `/proc/pressure/memory`, if available.
    pub host_psi: Option<MemoryPressure>,
    /// Own non-root cgroup `memory.pressure`, if available. Measures reclaim stalls experienced by
    /// `cladefsd` itself without sibling VM cgroups' `memory.high` throttling propagating into
    /// `/proc/pressure/memory`.
    pub cgroup_psi: Option<MemoryPressure>,
    /// Free host swap (zram or disk swap) from `/proc/meminfo` (`SwapFree:`), in bytes.
    pub host_swap_free: u64,
    /// Total host swap (zram or disk swap) from `/proc/meminfo` (`SwapTotal:`), in bytes.
    pub host_swap_total: u64,
}

/// The host's memory now (Linux: /proc/meminfo, and the daemon's cgroups').
#[cfg(target_os = "linux")]
pub fn host_memory() -> std::io::Result<HostMemory> {
    Ok(memory_sample()?.host)
}

/// The host's and daemon cgroup's memory and PSI pressure now (Linux).
#[cfg(target_os = "linux")]
pub fn memory_sample() -> std::io::Result<MemorySample> {
    let meminfo = std::fs::read_to_string("/proc/meminfo")?;
    let (total, host_available) = meminfo_of(&meminfo).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "/proc/meminfo: no MemTotal, MemAvailable",
        )
    })?;
    let host_swap_free = swap_free_of(&meminfo);
    let host_swap_total = swap_total_of(&meminfo);
    let cgroups = std::fs::read_to_string("/proc/self/cgroup")?;
    let mut cgroup_available: Option<u64> = None;
    let mut cgroup_psi: Option<MemoryPressure> = None;
    if let Some(path) = cgroup_v2_path_of(&cgroups) {
        let trimmed = path.trim_matches('/');
        if !trimmed.is_empty() {
            let leaf = std::path::Path::new("/sys/fs/cgroup").join(trimmed);
            cgroup_psi = read_psi(&leaf.join("memory.pressure"));
        }
        let mut dir = std::path::Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
        loop {
            if let Some(room) = cgroup_room(&dir)? {
                cgroup_available = Some(cgroup_available.map_or(room, |best| best.min(room)));
            }
            if !dir.pop() || dir == std::path::Path::new("/sys/fs") {
                break;
            }
        }
    }
    let host_psi = read_psi(std::path::Path::new("/proc/pressure/memory"));
    let available = host_available.min(cgroup_available.unwrap_or(u64::MAX));
    Ok(MemorySample {
        host: HostMemory { total, available },
        host_available,
        cgroup_available,
        host_psi,
        cgroup_psi,
        host_swap_free,
        host_swap_total,
    })
}

/// Reads and parses a Linux PSI file (`/proc/pressure/memory` or `<cgroup>/memory.pressure`).
/// Missing or unsupported PSI (kernels without `CONFIG_PSI`) returns `None`.
#[cfg(target_os = "linux")]
fn read_psi(path: &std::path::Path) -> Option<MemoryPressure> {
    // Missing or unsupported PSI on kernels without CONFIG_PSI (or restricted cgroups): fall back
    // to watermark-based governance rather than failing the governor tick.
    let text = std::fs::read_to_string(path).ok()?;
    psi_of(&text)
}

/// The host's memory now (macOS: what it says is available, `kern.memorystatus_level`).
#[cfg(target_os = "macos")]
pub fn host_memory() -> std::io::Result<HostMemory> {
    let total = sysctl_u64(c"hw.memsize")?;
    // cladefs: the share of it macOS says is available (`kern.memorystatus_level`, a percentage:
    // what its memory pressure is reckoned from), its inactive and purgeable pages among it, which
    // the kernel takes back as it needs. Free and speculative pages alone, as before, said a busy
    // Mac's memory was full, its daemon making no cell more (board 13065).
    let level = sysctl_u64(c"kern.memorystatus_level")?.min(100);
    Ok(HostMemory { total, available: total / 100 * level })
}

/// The host's memory sample on macOS (no cgroups or Linux PSI).
#[cfg(target_os = "macos")]
pub fn memory_sample() -> std::io::Result<MemorySample> {
    let host = host_memory()?;
    Ok(MemorySample {
        host,
        host_available: host.available,
        cgroup_available: None,
        host_psi: None,
        cgroup_psi: None,
        host_swap_free: 0,
        host_swap_total: 0,
    })
}

#[cfg(target_os = "macos")]
fn sysctl_u64(name: &std::ffi::CStr) -> std::io::Result<u64> {
    let mut value = [0u8; 8];
    let mut len = value.len();
    // SAFETY: a sysctl read into `value`, of `len` bytes, which it says how many it filled.
    let read = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(match len {
        4 => u32::from_ne_bytes(value[..4].try_into().unwrap()).into(),
        _ => u64::from_ne_bytes(value),
    })
}

/// Parses Linux PSI memory pressure text (`/proc/pressure/memory` or `<cgroup>/memory.pressure`).
/// Returns `None` if no valid `some` line is present; a missing `full` line defaults its fields to
/// zero.
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub fn psi_of(text: &str) -> Option<MemoryPressure> {
    let parse_line = |prefix: &str| -> Option<(u32, u64)> {
        let rest = text.lines().find_map(|line| line.strip_prefix(prefix))?;
        let mut avg10: Option<u32> = None;
        let mut total: Option<u64> = None;
        for token in rest.split_whitespace() {
            if let Some(value) = token.strip_prefix("avg10=") {
                avg10 = Some(parse_pct_bps(value)?);
            } else if let Some(value) = token.strip_prefix("total=") {
                // Malformed PSI field: ignore this line.
                total = Some(value.parse().ok()?);
            }
        }
        Some((avg10?, total?))
    };
    let (some_avg10_bps, some_total_us) = parse_line("some ")?;
    let (full_avg10_bps, full_total_us) = parse_line("full ").unwrap_or((0, 0));
    Some(MemoryPressure { some_avg10_bps, full_avg10_bps, some_total_us, full_total_us })
}

/// Parses a non-negative PSI percentage string (e.g. `"12.34"`, `"0.5"`, `"100.00"`) into basis
/// points (`0..=10_000`), rejecting values outside `0..=100.00` or containing non-digits.
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
fn parse_pct_bps(value: &str) -> Option<u32> {
    let (whole_str, frac_str) = match value.split_once('.') {
        Some((w, f)) => (w, f),
        None => (value, ""),
    };
    if whole_str.is_empty() || !whole_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !frac_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Malformed or overflowing integer percentage: reject.
    let whole: u32 = whole_str.parse().ok()?;
    if whole > 100 {
        return None;
    }
    let frac_bytes = frac_str.as_bytes();
    let d1 = frac_bytes.first().map_or(0u32, |b| u32::from(b.saturating_sub(b'0')));
    let d2 = frac_bytes.get(1).map_or(0u32, |b| u32::from(b.saturating_sub(b'0')));
    let frac = d1.checked_mul(10)?.checked_add(d2)?;
    let bps = whole.checked_mul(100)?.checked_add(frac)?;
    (bps <= 10_000).then_some(bps)
}

/// /proc/meminfo's MemTotal and MemAvailable, in bytes.
pub fn meminfo_of(meminfo: &str) -> Option<(u64, u64)> {
    let field = |name: &str| {
        let line = meminfo.lines().find_map(|line| line.strip_prefix(name))?;
        // Not a number: not the field.
        let kib: u64 = line.trim().strip_suffix("kB")?.trim().parse().ok()?;
        Some(kib * 1024)
    };
    Some((field("MemTotal:")?, field("MemAvailable:")?))
}

/// /proc/meminfo's `SwapFree:` in bytes (0 if absent or unparseable).
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub fn swap_free_of(meminfo: &str) -> u64 {
    meminfo_field(meminfo, "SwapFree:").unwrap_or(0)
}

/// /proc/meminfo's `SwapTotal:` in bytes (0 if absent or unparseable).
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub fn swap_total_of(meminfo: &str) -> u64 {
    meminfo_field(meminfo, "SwapTotal:").unwrap_or(0)
}

#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
fn meminfo_field(meminfo: &str, name: &str) -> Option<u64> {
    let line = meminfo.lines().find_map(|line| line.strip_prefix(name))?;
    // Malformed meminfo value: treat as absent.
    let kib = line.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()?;
    Some(kib.saturating_mul(1024))
}

/// Whether `/sys/kernel/mm/lru_gen/enabled` reports MGLRU enabled (non-zero hex `0x...` or
/// decimal mask).
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub fn mglru_enabled_of(raw: &str) -> bool {
    let trimmed = raw.trim();
    let parsed =
        if let Some(hex) = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")) {
            // Malformed hex mask: treat MGLRU as disabled.
            u64::from_str_radix(hex, 16).ok()
        } else {
            // Malformed decimal mask: treat MGLRU as disabled.
            trimmed.parse::<u64>().ok()
        };
    parsed.is_some_and(|mask| mask != 0)
}

/// Whether `/proc/self/status` `CapEff:` includes `CAP_DAC_OVERRIDE` (bit 1), required to write
/// proactive `memory.reclaim` to connected VM cgroups owned by other users/systemd slices.
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub fn has_cap_dac_override(proc_status: &str) -> bool {
    const CAP_DAC_OVERRIDE_BIT: u64 = 1 << 1;
    let Some(hex) = proc_status.lines().find_map(|line| line.strip_prefix("CapEff:")) else {
        return false;
    };
    // Malformed capability bitmask: treat capability as absent.
    u64::from_str_radix(hex.trim(), 16).ok().is_some_and(|mask| mask & CAP_DAC_OVERRIDE_BIT != 0)
}

/// Extracts the unified cgroup v2 relative path (`0::<path>`) from `/proc/<pid>/cgroup`.
/// Rejects relative traversal (`..`) and returns `None` for the root cgroup (`/`).
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub fn cgroup_v2_path_of(proc_cgroup: &str) -> Option<&str> {
    let path = proc_cgroup.lines().find_map(|line| line.strip_prefix("0::"))?.trim();
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() || trimmed.split('/').any(|part| part.is_empty() || part == "..") {
        return None;
    }
    Some(trimmed)
}

/// Per-VM cgroup memory observation used by the TMO proactive reclaim policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VmCgroupStats {
    /// Current resident memory (`memory.current`), in bytes.
    pub current_bytes: u64,
    /// Current swap usage (`memory.swap.current`), in bytes.
    pub swap_current_bytes: u64,
    /// Finite swap cap (`memory.swap.max`), or `None` for `max` (unlimited).
    pub swap_max_bytes: Option<u64>,
    /// Cgroup memory PSI (`memory.pressure`), if available.
    pub psi: Option<MemoryPressure>,
}

/// Maximum VM cgroup PSI `some avg10` (1.00%) below which proactive `memory.reclaim` will nudge
/// cold anonymous guest RAM into swap/zram. Above this, the VM's working set is active.
pub const VM_RECLAIM_PSI_LIMIT_BPS: u32 = 100;

/// Minimum host free swap (`SwapFree`) reserved for emergency kernel reclaim (256 MiB floor).
pub const VM_RECLAIM_MIN_SWAP_FREE: u64 = 256 << 20;

/// Maximum total bytes requested across all VM cgroups in one 1-second proactive reclaim pass
/// (128 MiB/s), further bounded by the current host headroom deficit (`headroom - available`).
pub const VM_RECLAIM_PASS_MAX: u64 = 128 << 20;

/// Maximum bytes requested per VM cgroup per proactive reclaim pass (64 MiB/s).
pub const VM_RECLAIM_STEP_MAX: u64 = 64 << 20;

/// Minimum non-zero reclaim step worth writing to `memory.reclaim` (4 MiB).
pub const VM_RECLAIM_STEP_MIN: u64 = 4 << 20;

/// Minimum interval between budgeted background `cachestat(2)` cold-part harvesting scans.
#[cfg(all(target_os = "linux", not(madsim)))]
const COLD_HARVEST_INTERVAL: Duration = Duration::from_secs(5);

/// Maximum cached extents inspected per background `cachestat(2)` cold-part scan (round-robin).
#[cfg(all(target_os = "linux", not(madsim)))]
const COLD_HARVEST_SCAN_BUDGET: usize = 64;

/// Maximum cold MGLRU-swapped DAX bytes harvested per background `cachestat(2)` scan.
#[cfg(all(target_os = "linux", not(madsim)))]
const COLD_HARVEST_MAX_BYTES: u64 = 64 << 20;

/// Minimum interval between proactive VM `memory.reclaim` passes.
#[cfg(all(target_os = "linux", not(madsim)))]
const VM_RECLAIM_INTERVAL: Duration = Duration::from_secs(1);

/// Initial and maximum per-cgroup exponential backoff (seconds) when a `memory.reclaim` write
/// makes zero progress or returns an error.
#[cfg(all(target_os = "linux", not(madsim)))]
const VM_RECLAIM_BACKOFF_MIN_SECS: u64 = 2;
#[cfg(all(target_os = "linux", not(madsim)))]
const VM_RECLAIM_BACKOFF_MAX_SECS: u64 = 32;

/// How long `cladefsd` holds its own cache growth after proactive VM reclaim makes progress so
/// the daemon does not immediately expand into the host RAM just reclaimed from VMs.
#[cfg(all(target_os = "linux", not(madsim)))]
const VM_RECLAIM_HOLD_DURATION: Duration = Duration::from_secs(5);

/// Host swap reserve kept free for emergency kernel reclaim: one eighth of `SwapTotal`, at least
/// [`VM_RECLAIM_MIN_SWAP_FREE`] (256 MiB).
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub fn vm_reclaim_swap_reserve(host_swap_total: u64) -> u64 {
    (host_swap_total / 8).max(VM_RECLAIM_MIN_SWAP_FREE)
}

/// Total bytes proactive VM reclaim may request in one 1-second pass: bounded by the current
/// headroom deficit (`headroom - host_available`) and [`VM_RECLAIM_PASS_MAX`].
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub fn vm_reclaim_pass_budget(headroom: u64, host_available: u64) -> u64 {
    let deficit = headroom.saturating_sub(host_available).min(VM_RECLAIM_PASS_MAX);
    let aligned = (deficit / SLAB).saturating_mul(SLAB);
    if aligned < VM_RECLAIM_STEP_MIN { 0 } else { aligned }
}

/// Computes how many bytes to request from a connected VM cgroup's `memory.reclaim` on one
/// proactive pass (Meta TMO pattern): reclaims cold anonymous RAM only from VMs with observable
/// low PSI when both the host and the VM's cgroup have swap headroom above reserve.
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub fn vm_reclaim_target(
    vm: VmCgroupStats,
    host_swap_free: u64,
    host_swap_total: u64,
    max_step: u64,
) -> u64 {
    let reserve = vm_reclaim_swap_reserve(host_swap_total);
    if host_swap_free <= reserve || max_step < VM_RECLAIM_STEP_MIN {
        return 0;
    }
    // Require observable cgroup PSI before proactively reclaiming a VM; without PSI we cannot
    // tell whether the guest's working set is actively faulting.
    let Some(psi) = vm.psi else {
        return 0;
    };
    if psi.some_avg10_bps >= VM_RECLAIM_PSI_LIMIT_BPS || psi.full_avg10_bps > 0 {
        return 0;
    }
    let host_room = host_swap_free.saturating_sub(reserve);
    let cgroup_swap_room = match vm.swap_max_bytes {
        Some(max) => max.saturating_sub(vm.swap_current_bytes),
        None => u64::MAX,
    };
    let vm_share = vm.current_bytes / 8;
    let raw = max_step.min(host_room).min(cgroup_swap_room).min(vm_share);
    let aligned = (raw / SLAB).saturating_mul(SLAB);
    if aligned < VM_RECLAIM_STEP_MIN { 0 } else { aligned }
}

/// What the cgroup at `dir` leaves room for, if it is limited: its limit (memory.max) less what it
/// holds (memory.current), of which its inactive files' pages (memory.stat's inactive_file) are
/// reclaimable.
#[cfg(target_os = "linux")]
fn cgroup_room(dir: &std::path::Path) -> std::io::Result<Option<u64>> {
    let read = |file: &str| match std::fs::read_to_string(dir.join(file)) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    };
    let (Some(max), Some(current), Some(stat)) =
        (read("memory.max")?, read("memory.current")?, read("memory.stat")?)
    else {
        return Ok(None);
    };
    Ok(room_of(&max, &current, &stat))
}

/// A cgroup's room, of its memory.max, memory.current and memory.stat: none if unlimited.
pub fn room_of(max: &str, current: &str, stat: &str) -> Option<u64> {
    // max, unlimited, is no number: no room said.
    let max: u64 = max.trim().parse().ok()?;
    // Not a number: no room said, as for a cgroup unlimited.
    let current: u64 = current.trim().parse().ok()?;
    // Not a number: none inactive, as a kernel without the field has none.
    let inactive: u64 = stat
        .lines()
        .find_map(|line| line.strip_prefix("inactive_file "))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    Some((max + inactive).saturating_sub(current))
}

/// What the daemon may hold: its `bytes`, and whether the host is `full`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Allowance {
    /// What the daemon may hold, its floor at least.
    pub bytes: u64,
    /// Whether the host is full: the room for the daemon under its floor.
    pub full: bool,
}

/// What the daemon may hold, holding `held`, of `host`, leaving `headroom`, and `floor` at least
/// ([`floor`]): whole slabs, one at least. cladefs: and whether the host is full, the room under
/// the floor, `was` it as the last look found it: entered under the floor, left only past it and a
/// quarter more, so that a host at its edge doesn't refuse and take cells by turns (board 5662).
pub fn allowance(held: u64, host: HostMemory, headroom: u64, floor: u64, was: bool) -> Allowance {
    let room = (held + host.available).saturating_sub(headroom);
    let full = match was {
        true => room < floor + floor / 4,
        false => room < floor,
    };
    Allowance { bytes: (room.max(floor) / SLAB * SLAB).max(SLAB), full }
}

/// Each tenant's share of `target` bytes by its weight, of `tenants`' (ID, weight): what it keeps
/// under pressure while it uses it. A tenant of weight 0 has none.
pub fn shares(target: u64, tenants: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let total: u128 = tenants.iter().map(|(_, weight)| u128::from(*weight)).sum();
    tenants
        .iter()
        .map(|&(tenant, weight)| {
            let share = match total {
                0 => 0,
                total => (u128::from(target) * u128::from(weight) / total) as u64,
            };
            (tenant, share)
        })
        .collect()
}

/// Starts `daemon`'s memory governor on its runtime for the rest of the daemon's life, the host's
/// support for it checked by the doctor first, which refuses to start the daemon without it
/// ([`governed_checks`], board 14217), rather than silently degrading to a broken split-brain
/// configuration.
/// cladefs: a supervised service on the daemon's runtime ([`super::run::service`]), as its shrinks
/// wait for the pages they evict holding no thread (board 2946).
pub(super) fn start_on(daemon: &Arc<Daemon>) -> Result<(), String> {
    let host = host_memory().map_err(|err| format!("reading the host's memory: {err}"))?;
    let limit = limit(host.total);
    let (headroom, floor) = (headroom(limit)?, floor(limit)?);
    // cladefs: its host's support checked by the doctor before the daemon started (board 14217).
    daemon.governor.floor.store(floor, Ordering::Relaxed);
    // Unify periodic memory governance with reactive slab relief onto one OS eviction thread
    // outside madsim/Shuttle (board 11703), avoiding a second eviction loop competing with relief.
    #[cfg(not(madsim))]
    {
        daemon.host.cache().relieve_pressure();
        let weak = Arc::downgrade(daemon);
        let mut tick = GovernorLoop::new(daemon.host.cache().refaults(), headroom, floor);
        let mut started = false;
        if daemon.host.slabs().attach_governor(INTERVAL, move |service_relief| {
            if !started {
                started = true;
                // Triggered on the unified eviction thread under `Awaited("memory governor")` so
                // supervision tests and runtime panic aborts still verify the live governor path
                // (hotbed review 140 finding 521).
                fail::fail_point!("cladefsd::service::memory governor");
            }
            let Some(daemon) = weak.upgrade() else { return };
            if let Some((allowance, mode)) = tick.sample_and_decide(&daemon) {
                memory::govern_with_sync_on(&daemon, allowance, mode, service_relief);
            }
        }) {
            return Ok(());
        }
    }
    let governing = Arc::clone(daemon);
    crate::run::service(daemon, "memory governor", move || govern_every(governing, headroom, floor))
        .map_err(|err| format!("starting the memory governor: {err}"))
}

/// Whether this host satisfies Linux governed-mode prerequisites (`require_host_support`).
/// Used by test harnesses (`guest/tests`) to pass an explicit `CLADEFSD_MEMORY` budget on hosts
/// without MGLRU/governor support while production `cladefsd` continues to fail hard in governed
/// mode (board 13373, board 13393).
pub fn host_supported() -> bool {
    #[cfg(all(target_os = "linux", not(madsim)))]
    {
        let Ok(host) = host_memory() else {
            return false;
        };
        let Ok(floor) = floor(limit(host.total)) else {
            return false;
        };
        require_host_support(floor).is_ok()
    }
    #[cfg(not(all(target_os = "linux", not(madsim))))]
    {
        true
    }
}

/// The host's support for governed mode on Linux (board 14217's doctor runs these, every one, before
/// the daemon starts, refusing if any fails): `cachestat(2)`; MGLRU enabled; host swap
/// (`SwapTotal >= floor`); and cgroup v2 reclaim permissions, a writable cgroup2 `/sys/fs/cgroup`
/// plus either `CAP_DAC_OVERRIDE` or explicit `CLADEFSD_VM_CGROUP_DELEGATED=1` when VM cgroups are
/// delegated to the daemon's user. Each check's name, and what it found, or what is wrong.
#[cfg(all(target_os = "linux", not(madsim)))]
pub fn governed_checks(floor: u64) -> Vec<(&'static str, Result<String, String>)> {
    let cachestat = cladefs_core::slabs::require_cachestat()
        .map(|()| "cachestat(2) works".to_owned())
        .map_err(|err| format!("{err} (Linux 6.5 or later)"));
    let mglru = match std::fs::read_to_string("/sys/kernel/mm/lru_gen/enabled") {
        Err(err) => Err(format!(
            "reading /sys/kernel/mm/lru_gen/enabled: {err} (Linux MGLRU is required for unified \
             memory governance)"
        )),
        Ok(raw) if mglru_enabled_of(&raw) => Ok(format!("enabled ({})", raw.trim())),
        Ok(_) => Err("MGLRU is disabled (/sys/kernel/mm/lru_gen/enabled=0); run `cladefsd \
             --setup-host` or `echo y > /sys/kernel/mm/lru_gen/enabled`"
            .into()),
    };
    let swap = std::fs::read_to_string("/proc/meminfo")
        .map_err(|err| format!("reading /proc/meminfo: {err}"))
        .and_then(|meminfo| match swap_total_of(&meminfo) {
            total if total >= floor => Ok(format!("{total} bytes, the floor {floor}")),
            total => Err(format!(
                "{total} bytes of swap (SwapTotal), below the governed floor {floor} bytes; run \
                 `cladefsd --setup-host` to provision zram or configure swap"
            )),
        });
    let delegated = node_local::env::var("CLADEFSD_VM_CGROUP_DELEGATED")
        .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"));
    let reclaim = match delegated {
        true => Ok("VM cgroups delegated (CLADEFSD_VM_CGROUP_DELEGATED)".to_owned()),
        false => std::fs::read_to_string("/proc/self/status")
            .map_err(|err| format!("reading /proc/self/status: {err}"))
            .and_then(|status| match has_cap_dac_override(&status) {
                true => Ok("CAP_DAC_OVERRIDE held".to_owned()),
                false => Err("missing CAP_DAC_OVERRIDE for proactive VM memory.reclaim (set \
                     AmbientCapabilities=CAP_CHOWN CAP_FOWNER CAP_KILL CAP_DAC_OVERRIDE, or \
                     delegate VM cgroups to the daemon user and set \
                     CLADEFSD_VM_CGROUP_DELEGATED=1)"
                    .to_owned()),
            }),
    };
    vec![
        ("cachestat", cachestat),
        ("MGLRU", mglru),
        ("swap", swap),
        ("cgroup2 at /sys/fs/cgroup", cgroup2_writable()),
        ("VM memory reclaim", reclaim),
    ]
}

/// Whether `/sys/fs/cgroup` is cgroup v2, mounted writable: VM reclaim finds VM cgroups by their
/// cgroup v2 paths alone (`cgroup_v2_path_of`), so that a v1 or hybrid mount, which passed before,
/// left it reclaiming nothing (board 14217).
#[cfg(all(target_os = "linux", not(madsim)))]
fn cgroup2_writable() -> Result<String, String> {
    // SAFETY: `libc::statfs` is a plain C struct where an all-zero bit pattern is valid.
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: a valid NUL-terminated path and an initialized statfs buffer.
    if unsafe { libc::statfs(c"/sys/fs/cgroup".as_ptr(), &raw mut fs) } != 0 {
        return Err(format!("statfs(/sys/fs/cgroup): {}", std::io::Error::last_os_error()));
    }
    if fs.f_type as i64 != libc::CGROUP2_SUPER_MAGIC as i64 {
        return Err(format!(
            "/sys/fs/cgroup is not cgroup v2 (its filesystem's magic is {:#x}): boot with \
             systemd.unified_cgroup_hierarchy=1",
            fs.f_type
        ));
    }
    // SAFETY: `libc::statvfs` is a plain C struct where an all-zero bit pattern is valid.
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a valid NUL-terminated path and an initialized statvfs buffer.
    if unsafe { libc::statvfs(c"/sys/fs/cgroup".as_ptr(), &raw mut vfs) } != 0 {
        return Err(format!("statvfs(/sys/fs/cgroup): {}", std::io::Error::last_os_error()));
    }
    if (vfs.f_flag & libc::ST_RDONLY) != 0 {
        return Err("/sys/fs/cgroup is mounted read-only (set ProtectControlGroups=no and \
             ReadWritePaths=... /sys/fs/cgroup)"
            .into());
    }
    Ok("cgroup v2, writable".into())
}

/// Governed mode's prerequisites, as [`governed_checks`] finds them: the first failing.
#[cfg(all(target_os = "linux", not(madsim)))]
pub fn require_host_support(floor: u64) -> Result<(), String> {
    match governed_checks(floor).into_iter().find(|(_, found)| found.is_err()) {
        Some((check, Err(err))) => Err(format!("memory governor: {check}: {err}")),
        _ => Ok(()),
    }
}

/// Idempotent root pre-start host setup (`cladefsd --setup-host`, e.g. systemd
/// `ExecStartPre=+/usr/local/bin/cladefsd --setup-host`): verifies `cachestat(2)`, enables MGLRU
/// and sets `min_ttl_ms` if zero, and provisions a zstd zram swap device (`MemTotal / 2`) if host
/// swap is below the governed floor.
#[cfg(all(target_os = "linux", not(madsim)))]
pub fn setup_host() -> Result<(), String> {
    use std::process::Command;

    struct ZramCleanup(Option<u32>);
    impl Drop for ZramCleanup {
        fn drop(&mut self) {
            if let Some(dev_num) = self.0 {
                // Best-effort removal of a newly hot-added zram device if setup fails partway.
                let _ =
                    std::fs::write("/sys/class/zram-control/hot_remove", format!("{dev_num}\n"));
            }
        }
    }

    cladefs_core::slabs::require_cachestat().map_err(|err| format!("setup-host: {err}"))?;

    let lru_path = std::path::Path::new("/sys/kernel/mm/lru_gen/enabled");
    let lru_raw = std::fs::read_to_string(lru_path)
        .map_err(|err| format!("setup-host: reading {}: {err}", lru_path.display()))?;
    if !mglru_enabled_of(&lru_raw) {
        std::fs::write(lru_path, "y\n").map_err(|err| {
            format!("setup-host: enabling MGLRU at {}: {err}", lru_path.display())
        })?;
        log::info!("cladefsd --setup-host: enabled MGLRU");
    }

    let ttl_path = std::path::Path::new("/sys/kernel/mm/lru_gen/min_ttl_ms");
    if let Ok(ttl_raw) = std::fs::read_to_string(ttl_path)
        && ttl_raw.trim() == "0"
    {
        std::fs::write(ttl_path, "1000\n")
            .map_err(|err| format!("setup-host: setting {} to 1000: {err}", ttl_path.display()))?;
        log::info!("cladefsd --setup-host: set lru_gen/min_ttl_ms=1000");
    }

    let meminfo = std::fs::read_to_string("/proc/meminfo")
        .map_err(|err| format!("setup-host: reading /proc/meminfo: {err}"))?;
    let (total, _) = meminfo_of(&meminfo)
        .ok_or_else(|| "setup-host: /proc/meminfo missing MemTotal/MemAvailable".to_owned())?;
    let required = floor(limit(total))?;
    let swap_total = swap_total_of(&meminfo);
    if swap_total >= required {
        log::info!(
            "cladefsd --setup-host: host ready (SwapTotal={swap_total} >= floor={required})"
        );
        return Ok(());
    }

    let hot_add = std::path::Path::new("/sys/class/zram-control/hot_add");
    if !hot_add.exists() {
        let status = Command::new("modprobe")
            .arg("zram")
            .status()
            .map_err(|err| format!("setup-host: running modprobe zram: {err}"))?;
        if !status.success() {
            return Err(format!("setup-host: modprobe zram failed with {status}"));
        }
    }
    let dev_num = std::fs::read_to_string(hot_add)
        .map_err(|err| format!("setup-host: allocating zram via {}: {err}", hot_add.display()))?;
    let dev_num: u32 = dev_num
        .trim()
        .parse()
        .map_err(|err| format!("setup-host: invalid zram device number {dev_num:?}: {err}"))?;
    let mut cleanup = ZramCleanup(Some(dev_num));
    let sys_dev = std::path::PathBuf::from(format!("/sys/block/zram{dev_num}"));
    let dev_path = format!("/dev/zram{dev_num}");

    let comp_path = sys_dev.join("comp_algorithm");
    if let Err(err) = std::fs::write(&comp_path, "zstd\n") {
        // Fallback to lz4 only if zstd isn't compiled into the host kernel's crypto/zram module.
        if err.raw_os_error() == Some(libc::EINVAL) {
            std::fs::write(&comp_path, "lz4\n")
                .map_err(|e| format!("setup-host: setting {} to lz4: {e}", comp_path.display()))?;
        } else {
            return Err(format!("setup-host: setting {} to zstd: {err}", comp_path.display()));
        }
    }

    // Reserve one extra slab above `required` so `mkswap`'s swap header page never leaves
    // `SwapTotal` one page short of `floor`.
    let disksize = ((total / 2) / SLAB * SLAB).max(required.saturating_add(SLAB));
    let size_path = sys_dev.join("disksize");
    std::fs::write(&size_path, format!("{disksize}\n")).map_err(|err| {
        format!("setup-host: setting {} to {disksize}: {err}", size_path.display())
    })?;

    let mkswap = Command::new("mkswap")
        .arg(&dev_path)
        .status()
        .map_err(|err| format!("setup-host: running mkswap {dev_path}: {err}"))?;
    if !mkswap.success() {
        return Err(format!("setup-host: mkswap {dev_path} failed with {mkswap}"));
    }

    let swapon = Command::new("swapon")
        .args(["-p", "100", &dev_path])
        .status()
        .map_err(|err| format!("setup-host: running swapon -p 100 {dev_path}: {err}"))?;
    if !swapon.success() {
        return Err(format!("setup-host: swapon -p 100 {dev_path} failed with {swapon}"));
    }

    cleanup.0 = None;
    log::info!("cladefsd --setup-host: provisioned {dev_path} ({disksize} bytes, priority 100)");
    Ok(())
}

/// Starts the process's default daemon's memory governor.
#[cfg(not(madsim))]
pub fn start() -> Result<(), String> {
    start_on(Daemon::process())
}

/// Part refaults/sec at or above which the governor considers the extent cache actively thrashing
/// (~32 MiB/s of recently governor-evicted parts being immediately re-read from disk/ZFS).
pub const REFAULT_THRASH_ENTER: u64 = 32;

/// Part refaults/sec at or below which thrashing clears (hysteresis).
pub const REFAULT_THRASH_EXIT: u64 = 8;

/// Own-cgroup PSI `some avg10` threshold (5.00%) triggering mapped-extent reclaim.
const OWN_PSI_SOME_BPS: u32 = 500;
/// Own-cgroup PSI `full avg10` threshold (1.00%) triggering mapped-extent reclaim.
const OWN_PSI_FULL_BPS: u32 = 100;
/// Own-cgroup instantaneous `some` stall rate threshold in basis points (10.00% of elapsed tick).
const OWN_PSI_DELTA_BPS: u64 = 1_000;

/// Host-wide PSI `some avg10` threshold (10.00%), considered only once host `MemAvailable` falls
/// below `headroom / 2` so sibling cgroups' `memory.high` throttling does not evict `cladefsd`'s
/// hot DAX cache while the host still has ample memory.
const HOST_PSI_SOME_BPS: u32 = 1_000;
/// Host-wide PSI `full avg10` threshold (2.00%), considered only below `headroom / 2`.
const HOST_PSI_FULL_BPS: u32 = 200;
/// Host-wide instantaneous `some` stall rate threshold in basis points (20.00% of elapsed tick),
/// considered only below `headroom / 2`.
const HOST_PSI_DELTA_BPS: u64 = 2_000;

/// Minimum mapped-extent shrink step per tick when shrinking cold mapped extents under host or
/// own-cgroup PSI pressure.
pub const MAPPED_SHRINK_MIN: u64 = 64 << 20;
/// Maximum mapped-extent shrink step per tick (2.5 GiB/s max drain rate at 100 ms ticks), giving
/// the 1-second refault window time to detect hot working sets before dumping gigabytes of cache.
pub const MAPPED_SHRINK_MAX: u64 = 256 << 20;

/// Sliding 1-second wall-clock window of DAX part refault counts (parts/sec).
#[derive(Clone, Debug)]
pub struct RefaultWindow {
    last_total: u64,
    now_ms: u64,
    samples: VecDeque<(u64, u64, u64)>,
    sum: u64,
}

impl RefaultWindow {
    /// Initializes a window seeded with the cache's current monotonic refault count.
    pub fn new(initial_total: u64) -> Self {
        Self { last_total: initial_total, now_ms: 0, samples: VecDeque::new(), sum: 0 }
    }

    /// Records the latest monotonic refault count after `elapsed` wall-clock time and returns the
    /// refault rate in parts per second over the last second (normalized when ticks exceed 1 s).
    pub fn record_elapsed(&mut self, current_total: u64, elapsed: Duration) -> u64 {
        let elapsed_ms = elapsed.as_millis().clamp(1, u64::MAX as u128) as u64;
        self.now_ms = self.now_ms.saturating_add(elapsed_ms);
        let delta = current_total.saturating_sub(self.last_total);
        self.last_total = current_total;
        if delta > 0 {
            self.samples.push_back((self.now_ms, delta, elapsed_ms));
            self.sum = self.sum.saturating_add(delta);
        }
        while let Some(&(ts, old_delta, _)) = self.samples.front() {
            if self.now_ms.saturating_sub(ts) >= 1_000 {
                self.samples.pop_front();
                self.sum = self.sum.saturating_sub(old_delta);
            } else {
                break;
            }
        }
        let span_ms = match self.samples.front() {
            Some(&(first_ts, _, first_dur)) => {
                self.now_ms.saturating_sub(first_ts).saturating_add(first_dur).max(1_000)
            }
            None => 1_000,
        };
        if span_ms > 1_000 {
            (u128::from(self.sum).saturating_mul(1_000) / u128::from(span_ms)) as u64
        } else {
            self.sum
        }
    }

    /// Records the latest monotonic refault count assuming a nominal [`INTERVAL`] (100 ms) tick.
    pub fn record(&mut self, current_total: u64) -> u64 {
        self.record_elapsed(current_total, INTERVAL)
    }
}

/// How aggressively the governor reclaims cached memory on one tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReclaimMode {
    /// Plenty of memory: allow caches to grow up to `allowance`, no eviction.
    None,
    /// Soft headroom dip without real kernel reclaim pressure, or active refault thrashing above
    /// the emergency watermark: shed derived caches, spare slabs, and *idle (unmapped)* DAX
    /// extents toward `allowance`, and cap `total_cap` at the remaining mapped working set
    /// (`>= floor`) so `cladefsd` stops growing without surrendering hot mapped DAX extents in a
    /// ratchet loop.
    IdleOnly,
    /// Genuine memory pressure (own cgroup `memory.max` tight/emergency, own cgroup PSI active,
    /// or host low with active kernel memory pressure or emergency headroom): reclaim derived
    /// caches and idle extents immediately, and shrink mapped DAX extents by at most
    /// `max_shrink_bytes` toward `allowance` (never below `floor`).
    Mapped {
        /// Maximum mapped DAX bytes to evict in this tick (`u64::MAX` when own `memory.max` is
        /// below the emergency watermark `headroom / 4` to prevent cgroup OOM kills).
        max_shrink_bytes: u64,
    },
}

/// Hysteresis and previous-tick state carried across [`decide`] calls.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PolicyState {
    /// Whether the host was full on the previous tick.
    pub full: bool,
    /// Whether the extent cache was thrashing on the previous tick.
    pub thrashing: bool,
    /// Previous own-cgroup PSI `some_total_us` for instantaneous stall deltas.
    pub prev_cgroup_some_us: Option<u64>,
    /// Previous host-wide PSI `some_total_us` for instantaneous stall deltas.
    pub prev_host_some_us: Option<u64>,
}

/// The governor's decision for one tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision {
    /// Target memory allowance (`>= floor`, slab-aligned).
    pub allowance: u64,
    /// Reclaim aggressiveness for this tick.
    pub mode: ReclaimMode,
    /// Whether the host or cgroup is full (`room < floor` with hysteresis).
    pub full: bool,
    /// Whether the daemon is under memory pressure (holding above target, own PSI active, or
    /// actively thrashing).
    pub pressured: bool,
    /// Whether active DAX refault thrashing is detected.
    pub thrashing: bool,
}

fn psi_delta_us(psi: Option<MemoryPressure>, prev: &mut Option<u64>) -> u64 {
    match (psi, *prev) {
        (Some(p), Some(last)) => {
            *prev = Some(p.some_total_us);
            p.some_total_us.saturating_sub(last)
        }
        (Some(p), None) => {
            *prev = Some(p.some_total_us);
            0
        }
        (None, _) => {
            *prev = None;
            0
        }
    }
}

/// Computes the governor's reclaim and backpressure decision for one tick from the daemon's
/// governed bytes, host/cgroup memory and PSI sample, headroom, floor, 1-second refault rate, and
/// elapsed time since the previous tick.
pub fn decide(
    held: u64,
    sample: MemorySample,
    headroom: u64,
    floor: u64,
    refaults_per_sec: u64,
    elapsed: Duration,
    state: &mut PolicyState,
) -> Decision {
    let base = allowance(held, sample.host, headroom, floor, state.full);
    let thrashing = match state.thrashing {
        true => refaults_per_sec > REFAULT_THRASH_EXIT,
        false => refaults_per_sec >= REFAULT_THRASH_ENTER,
    };
    let elapsed_us = elapsed.as_micros().clamp(1, u64::MAX as u128);
    let own_delta_us = psi_delta_us(sample.cgroup_psi, &mut state.prev_cgroup_some_us);
    let host_delta_us = psi_delta_us(sample.host_psi, &mut state.prev_host_some_us);
    let own_delta_bps = u128::from(own_delta_us).saturating_mul(10_000) / elapsed_us;
    let host_delta_bps = u128::from(host_delta_us).saturating_mul(10_000) / elapsed_us;

    let critical_headroom = headroom / 2;
    let emergency_headroom = headroom / 4;
    let cgroup_emergency = sample.cgroup_available.is_some_and(|avail| avail < emergency_headroom);
    let cgroup_tight = sample.cgroup_available.is_some_and(|avail| avail < headroom);
    let own_psi_pressured = sample.cgroup_psi.is_some_and(|psi| {
        psi.some_avg10_bps >= OWN_PSI_SOME_BPS
            || psi.full_avg10_bps >= OWN_PSI_FULL_BPS
            || own_delta_bps >= u128::from(OWN_PSI_DELTA_BPS)
    });

    let host_emergency = sample.host.available < emergency_headroom;
    let host_reclaim_pressured = if sample.host.available < critical_headroom {
        match sample.host_psi {
            Some(psi) => {
                psi.some_avg10_bps >= HOST_PSI_SOME_BPS
                    || psi.full_avg10_bps >= HOST_PSI_FULL_BPS
                    || host_delta_bps >= u128::from(HOST_PSI_DELTA_BPS)
            }
            // Kernels without PSI (or macOS): wait until the critical watermark rather than
            // evicting hot mapped extents on every headroom dip.
            None => true,
        }
    } else {
        false
    };

    let excess = held.saturating_sub(base.bytes);
    let rate_limited = (excess / 4).clamp(MAPPED_SHRINK_MIN, MAPPED_SHRINK_MAX);
    let mode = if held <= base.bytes {
        ReclaimMode::None
    } else if cgroup_emergency {
        // Own cgroup `memory.max` critically low (< headroom / 4): shrink promptly to avoid
        // cgroup OOM kill, even while refaulting.
        ReclaimMode::Mapped { max_shrink_bytes: u64::MAX }
    } else if host_emergency {
        // Host `MemAvailable` critically low (< headroom / 4): never let `thrashing` veto host
        // give-back (prevents host OOM killing VMs while cladefsd sits at OOMScoreAdjust -900),
        // but step down at the bounded rate rather than dumping gigabytes on a transient spike.
        ReclaimMode::Mapped { max_shrink_bytes: rate_limited }
    } else if (cgroup_tight || own_psi_pressured || host_reclaim_pressured) && !thrashing {
        ReclaimMode::Mapped { max_shrink_bytes: rate_limited }
    } else {
        // Soft headroom dip without kernel reclaim pressure, or active refault thrashing above
        // the emergency watermark: shed derived caches, spares, and idle (unmapped) extents and
        // stop growing, keeping mapped DAX extents resident.
        ReclaimMode::IdleOnly
    };

    let pressured = held > base.bytes || own_psi_pressured || thrashing;
    state.full = base.full;
    state.thrashing = thrashing;

    Decision { allowance: base.bytes, mode, full: base.full, pressured, thrashing }
}

struct GovernorLoop {
    headroom: u64,
    floor: u64,
    policy: PolicyState,
    refaults: RefaultWindow,
    last_tick: std::time::Instant,
    #[cfg(all(target_os = "linux", not(madsim)))]
    last_cold_harvest: std::time::Instant,
    #[cfg(all(target_os = "linux", not(madsim)))]
    last_vm_reclaim: std::time::Instant,
}

impl GovernorLoop {
    fn new(initial_refaults: u64, headroom: u64, floor: u64) -> Self {
        let now = std::time::Instant::now();
        Self {
            headroom,
            floor,
            policy: PolicyState::default(),
            refaults: RefaultWindow::new(initial_refaults),
            last_tick: now,
            #[cfg(all(target_os = "linux", not(madsim)))]
            last_cold_harvest: now,
            #[cfg(all(target_os = "linux", not(madsim)))]
            last_vm_reclaim: now,
        }
    }

    fn sample_and_decide(&mut self, daemon: &Arc<Daemon>) -> Option<(u64, ReclaimMode)> {
        let now = std::time::Instant::now();
        let elapsed = now.saturating_duration_since(self.last_tick).max(Duration::from_millis(1));
        let sample = match memory_sample() {
            Ok(sample) => sample,
            Err(err) => {
                log::error!("cladefsd: reading the host's memory: {err}");
                return None;
            }
        };
        self.last_tick = now;
        daemon.host.cache().advance_ms(elapsed.as_millis().min(u64::MAX as u128) as u64);
        let refaults_per_sec =
            self.refaults.record_elapsed(daemon.host.cache().refaults(), elapsed);
        let was_full = daemon.governor.full.load(Ordering::Relaxed);
        let was_thrashing = daemon.governor.thrashing.load(Ordering::Relaxed);
        self.policy.full = was_full;
        let held = memory::governed_bytes_on(daemon);
        let decision = decide(
            held,
            sample,
            self.headroom,
            self.floor,
            refaults_per_sec,
            elapsed,
            &mut self.policy,
        );
        let reported_psi = sample.cgroup_psi.or(sample.host_psi).unwrap_or_default();
        daemon.governor.pressured.store(decision.pressured, Ordering::Relaxed);
        daemon.governor.thrashing.store(decision.thrashing, Ordering::Relaxed);
        daemon.governor.target.store(decision.allowance, Ordering::Relaxed);
        daemon.governor.refaults_per_sec.store(refaults_per_sec, Ordering::Relaxed);
        daemon
            .governor
            .psi_some_avg10_bps
            .store(u64::from(reported_psi.some_avg10_bps), Ordering::Relaxed);
        daemon
            .governor
            .psi_full_avg10_bps
            .store(u64::from(reported_psi.full_avg10_bps), Ordering::Relaxed);
        if decision.full && !was_full {
            daemon.governor.fulls.fetch_add(1, Ordering::Relaxed);
        }
        if decision.full != was_full {
            log::warn!(
                "cladefsd: the host's memory is {}: {} bytes available, the floor {}",
                if decision.full { "full, making no cell more" } else { "no longer full" },
                sample.host.available,
                self.floor
            );
        }
        if decision.thrashing != was_thrashing {
            log::warn!(
                "cladefsd: extent cache refault thrashing {}: {refaults_per_sec} refaults/s, \
                 {} bytes available, target {}, floor {}",
                if decision.thrashing { "detected, holding mapped extents" } else { "cleared" },
                sample.host.available,
                decision.allowance,
                self.floor
            );
        }
        daemon.governor.full.store(decision.full, Ordering::Relaxed);

        #[cfg(all(target_os = "linux", not(madsim)))]
        {
            if now.saturating_duration_since(self.last_cold_harvest) >= COLD_HARVEST_INTERVAL {
                self.last_cold_harvest = now;
                if !decision.thrashing
                    && (decision.pressured
                        || sample.host.available < self.headroom.saturating_mul(2))
                {
                    spawn_cold_harvest(daemon);
                }
            }
            if now.saturating_duration_since(self.last_vm_reclaim) >= VM_RECLAIM_INTERVAL {
                self.last_vm_reclaim = now;
                // Budget proactive VM reclaim strictly from raw host `MemAvailable`,
                // never `cladefsd`'s own cgroup limit (swapping sibling VMs cannot free
                // room inside `cladefsd`'s cgroup).
                let pass_budget = vm_reclaim_pass_budget(self.headroom, sample.host_available);
                if pass_budget > 0
                    && sample.host_swap_free > vm_reclaim_swap_reserve(sample.host_swap_total)
                {
                    spawn_vm_reclaim(
                        daemon,
                        sample.host_swap_free,
                        sample.host_swap_total,
                        pass_budget,
                    );
                }
            }
        }

        // While proactive VM reclaim is actively running or recently reclaimed guest RAM,
        // cap `cladefsd`'s growth at its current resident size so it does not ratchet
        // guest RAM into swap only to consume the freed RAM itself.
        #[cfg(all(target_os = "linux", not(madsim)))]
        let effective_allowance = if is_vm_reclaim_holding(daemon, now) {
            let held_aligned = held.div_ceil(SLAB).saturating_mul(SLAB).max(self.floor);
            decision.allowance.min(held_aligned)
        } else {
            decision.allowance
        };
        #[cfg(not(all(target_os = "linux", not(madsim))))]
        let effective_allowance = decision.allowance;

        Some((effective_allowance, decision.mode))
    }
}

/// cladefs: governs `daemon`'s memory now and every [`INTERVAL`] after, leaving `headroom` free
/// and keeping `floor` ([`start_on`]), used when no OS eviction thread runs (madsim / Shuttle).
async fn govern_every(daemon: Arc<Daemon>, headroom: u64, floor: u64) {
    let mut tick = GovernorLoop::new(daemon.host.cache().refaults(), headroom, floor);
    loop {
        if let Some((effective_allowance, mode)) = tick.sample_and_decide(&daemon) {
            memory::govern_with_on(&daemon, effective_allowance, mode).await;
        }
        tokio::time::sleep(INTERVAL).await;
    }
}

/// Runs one non-overlapping background blocking task guarded by an `AtomicBool` on `daemon`,
/// ensuring concurrent triggers skip rather than pile up and the flag always clears on exit/panic.
#[cfg(all(target_os = "linux", not(madsim)))]
fn spawn_non_overlapping(
    daemon: &Arc<Daemon>,
    busy: fn(&Daemon) -> &AtomicBool,
    work: impl FnOnce(&Daemon) + Send + 'static,
) {
    if busy(daemon).compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return;
    }
    let daemon = Arc::clone(daemon);
    let runtime = daemon.runtime().handle().clone();
    // Background Linux blocking pass (cachestat(2) / cgroup memory.reclaim), outside the simulation.
    runtime.spawn_blocking(move || {
        struct Clear<'a>(&'a AtomicBool);
        impl Drop for Clear<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _clear = Clear(busy(&daemon));
        work(&daemon);
    });
}

/// Runs one non-overlapping budgeted `cachestat(2)` cold-part harvest pass on a blocking worker
/// thread so syscalls never stall the eviction thread.
#[cfg(all(target_os = "linux", not(madsim)))]
fn spawn_cold_harvest(daemon: &Arc<Daemon>) {
    spawn_non_overlapping(
        daemon,
        |d| &d.governor.cold_harvest_busy,
        |daemon| {
            let harvested = daemon
                .host
                .cache()
                .evict_kernel_cold(COLD_HARVEST_SCAN_BUDGET, COLD_HARVEST_MAX_BYTES);
            if harvested > 0 {
                daemon.governor.kernel_cold_harvested_bytes.fetch_add(harvested, Ordering::Relaxed);
            }
        },
    );
}

#[cfg(all(target_os = "linux", not(madsim)))]
fn is_vm_reclaim_holding(daemon: &Daemon, now: std::time::Instant) -> bool {
    if daemon.governor.vm_reclaim_busy.load(Ordering::Acquire) {
        return true;
    }
    let hold_until = *daemon.governor.vm_reclaim_hold_until.lock().unwrap();
    hold_until.is_some_and(|until| now < until)
}

/// Runs one non-overlapping TMO proactive `memory.reclaim` pass across connected external VM
/// processes' cgroups on a blocking worker thread so synchronous kernel reclaim never stalls the
/// eviction thread. Completely orchestrator-agnostic: discovers each VM's cgroup from its
/// authenticated `SO_PEERPIDFD`.
#[cfg(all(target_os = "linux", not(madsim)))]
fn spawn_vm_reclaim(
    daemon: &Arc<Daemon>,
    host_swap_free: u64,
    host_swap_total: u64,
    pass_budget: u64,
) {
    spawn_non_overlapping(
        daemon,
        |d| &d.governor.vm_reclaim_busy,
        move |daemon| {
            reclaim_connected_vms(daemon, host_swap_free, host_swap_total, pass_budget);
        },
    );
}

#[cfg(all(target_os = "linux", not(madsim)))]
fn read_cgroup_u64(path: &std::path::Path) -> Option<u64> {
    // Missing cgroup file (process exited or memory controller absent): skip this cgroup.
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(all(target_os = "linux", not(madsim)))]
fn read_cgroup_limit(path: &std::path::Path) -> Option<Option<u64>> {
    // Missing cgroup file (process exited or swap controller absent): skip this cgroup.
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    if trimmed == "max" {
        return Some(None);
    }
    // Unparseable numeric limit: ignore this cgroup rather than guessing.
    Some(Some(trimmed.parse().ok()?))
}

#[cfg(all(target_os = "linux", not(madsim)))]
fn effective_cgroup_swap_room(leaf: &std::path::Path) -> Option<u64> {
    let mut dir = leaf.to_path_buf();
    let mut tightest: Option<u64> = None;
    loop {
        if let Some(Some(max)) = read_cgroup_limit(&dir.join("memory.swap.max")) {
            let cur = read_cgroup_u64(&dir.join("memory.swap.current")).unwrap_or(0);
            let room = max.saturating_sub(cur);
            tightest = Some(tightest.map_or(room, |best| best.min(room)));
        }
        if !dir.pop()
            || dir == std::path::Path::new("/sys/fs/cgroup")
            || dir == std::path::Path::new("/sys/fs")
        {
            return tightest;
        }
    }
}

#[cfg(all(target_os = "linux", not(madsim)))]
fn write_memory_reclaim(path: &std::path::Path, cmd: &[u8]) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // Open write-only without O_CREAT so a vanishing cgroup fails ENOENT cleanly.
    let file = std::fs::OpenOptions::new().write(true).open(path)?;
    // Issue a single raw write(2) without std::io::Write's EINTR retry loop: Linux stops
    // memory.reclaim early on signal rather than expecting silent retries.
    // SAFETY: `file` is a valid open fd and `cmd` points to `cmd.len()` readable bytes.
    let rc = unsafe { libc::write(file.as_raw_fd(), cmd.as_ptr().cast(), cmd.len()) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(all(target_os = "linux", not(madsim)))]
fn record_vm_reclaim_backoff(daemon: &Daemon, rel: &str, now: std::time::Instant) {
    let mut map = daemon.governor.vm_reclaim_backoff.lock().unwrap();
    let entry = map
        .entry(rel.to_owned())
        .and_modify(|b| {
            b.delay_secs = b.delay_secs.saturating_mul(2).min(VM_RECLAIM_BACKOFF_MAX_SECS);
            b.next_try = now + Duration::from_secs(b.delay_secs);
        })
        .or_insert(VmReclaimBackoff {
            next_try: now + Duration::from_secs(VM_RECLAIM_BACKOFF_MIN_SECS),
            delay_secs: VM_RECLAIM_BACKOFF_MIN_SECS,
        });
    let _ = entry;
}

#[cfg(all(target_os = "linux", not(madsim)))]
fn reclaim_connected_vms(
    daemon: &Daemon,
    mut host_swap_free: u64,
    host_swap_total: u64,
    pass_budget: u64,
) {
    let cgroups = crate::server::connected_vm_cgroups(daemon);
    if cgroups.is_empty() || pass_budget < VM_RECLAIM_STEP_MIN {
        return;
    }
    // Unreadable own cgroup (unusual procfs): proceed with empty own path so we only skip root.
    let own_cgroup = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|s| cgroup_v2_path_of(&s).map(str::to_owned));

    let now = std::time::Instant::now();
    // Prune backoff entries for VMs that are no longer connected so the map stays bounded.
    {
        let live: std::collections::HashSet<&str> = cgroups.iter().map(String::as_str).collect();
        let mut map = daemon.governor.vm_reclaim_backoff.lock().unwrap();
        map.retain(|k, _| live.contains(k.as_str()));
    }

    let mut eligible = Vec::new();
    for rel in &cgroups {
        // Never reclaim cladefsd's own cgroup or an ancestor containing cladefsd.
        if let Some(own) = &own_cgroup
            && (own == rel || own.starts_with(&format!("{rel}/")))
        {
            continue;
        }
        {
            let map = daemon.governor.vm_reclaim_backoff.lock().unwrap();
            if map.get(rel.as_str()).is_some_and(|b| now < b.next_try) {
                continue;
            }
        }
        let dir = std::path::Path::new("/sys/fs/cgroup").join(rel);
        let Some(current_bytes) = read_cgroup_u64(&dir.join("memory.current")) else { continue };
        let swap_room = effective_cgroup_swap_room(&dir);
        let psi = read_psi(&dir.join("memory.pressure"));
        let stats =
            VmCgroupStats { current_bytes, swap_current_bytes: 0, swap_max_bytes: swap_room, psi };
        if vm_reclaim_target(stats, host_swap_free, host_swap_total, VM_RECLAIM_STEP_MAX) > 0 {
            eligible.push((rel.as_str(), dir, stats));
        }
    }
    if eligible.is_empty() {
        return;
    }

    let per_vm_cap =
        (pass_budget / eligible.len() as u64).clamp(VM_RECLAIM_STEP_MIN, VM_RECLAIM_STEP_MAX);
    let mut remaining_budget = pass_budget;
    let mut any_progress = false;

    for (rel, dir, stats) in eligible {
        if remaining_budget < VM_RECLAIM_STEP_MIN {
            break;
        }
        let step_cap = per_vm_cap.min(remaining_budget);
        let target = vm_reclaim_target(stats, host_swap_free, host_swap_total, step_cap);
        if target == 0 {
            continue;
        }
        let swap_path = dir.join("memory.swap.current");
        let cur_path = dir.join("memory.current");
        let reclaim_path = dir.join("memory.reclaim");
        let swap_before = read_cgroup_u64(&swap_path).unwrap_or(0);
        let cur_before = read_cgroup_u64(&cur_path).unwrap_or(stats.current_bytes);

        let mut mode = daemon.governor.vm_reclaim_swappiness_mode.load(Ordering::Relaxed);
        let res = loop {
            let cmd = match mode {
                0 => format!("{target} swappiness=max\n"),
                1 => format!("{target} swappiness=200\n"),
                _ => format!("{target}\n"),
            };
            let attempt = write_memory_reclaim(&reclaim_path, cmd.as_bytes());
            if mode < 2
                && attempt.as_ref().is_err_and(|err| err.raw_os_error() == Some(libc::EINVAL))
            {
                mode += 1;
                daemon.governor.vm_reclaim_swappiness_mode.store(mode, Ordering::Relaxed);
                continue;
            }
            break attempt;
        };

        daemon.governor.vm_reclaim_attempts.fetch_add(1, Ordering::Relaxed);
        let swap_after = read_cgroup_u64(&swap_path).unwrap_or(swap_before);
        let cur_after = read_cgroup_u64(&cur_path).unwrap_or(cur_before);
        let actual_reclaimed =
            swap_after.saturating_sub(swap_before).max(cur_before.saturating_sub(cur_after));

        match res {
            // Linux `memory.reclaim` returns 0 only when the kernel reclaimed at least `target`
            // bytes (`user_proactive_reclaim`); concurrent guest allocations or clean-page reclaim
            // can mask the `memory.swap.current` delta, so charge `actual_reclaimed.max(target)`.
            Ok(()) => {
                let reclaimed = actual_reclaimed.max(target);
                daemon.governor.vm_reclaim_bytes.fetch_add(reclaimed, Ordering::Relaxed);
                remaining_budget = remaining_budget.saturating_sub(reclaimed);
                host_swap_free = host_swap_free.saturating_sub(reclaimed);
                any_progress = true;
                daemon.governor.vm_reclaim_backoff.lock().unwrap().remove(rel);
            }
            // Linux memory.reclaim returns EAGAIN when reclaim stopped short of the full target
            // (including 0 bytes reclaimed) and EINTR if interrupted by a signal. Count only
            // measured progress and back off exponentially on 0-byte passes so we never spin.
            Err(err) if matches!(err.raw_os_error(), Some(libc::EAGAIN | libc::EINTR)) => {
                if actual_reclaimed > 0 {
                    daemon.governor.vm_reclaim_bytes.fetch_add(actual_reclaimed, Ordering::Relaxed);
                    remaining_budget = remaining_budget.saturating_sub(actual_reclaimed);
                    host_swap_free = host_swap_free.saturating_sub(actual_reclaimed);
                    any_progress = true;
                    daemon.governor.vm_reclaim_backoff.lock().unwrap().remove(rel);
                } else {
                    record_vm_reclaim_backoff(daemon, rel, std::time::Instant::now());
                }
            }
            Err(err) => {
                daemon.governor.vm_reclaim_errors.fetch_add(1, Ordering::Relaxed);
                record_vm_reclaim_backoff(daemon, rel, std::time::Instant::now());
                if matches!(err.raw_os_error(), Some(libc::EACCES | libc::EPERM | libc::EROFS)) {
                    if !daemon.governor.vm_reclaim_denied_logged.swap(true, Ordering::Relaxed) {
                        log::warn!(
                            "cladefsd: proactive VM memory.reclaim denied for {}: {err} \
                             (grant CAP_DAC_OVERRIDE and writable /sys/fs/cgroup, or delegate VM \
                             cgroups to the daemon user)",
                            reclaim_path.display()
                        );
                    }
                    break;
                }
            }
        }
    }

    if any_progress {
        *daemon.governor.vm_reclaim_hold_until.lock().unwrap() =
            Some(std::time::Instant::now() + VM_RECLAIM_HOLD_DURATION);
    }
}

/// cladefs: what the disk governor leaves free of a filesystem, as `CLADEFSD_DISK_HEADROOM` or
/// TOML `[disk].headroom` / `[[disks]].headroom` says.
pub use bucket_cell::host_disk::DiskHeadroom;

/// cladefs: the headroom `CLADEFSD_DISK_HEADROOM` gives, if any: bytes, or `N%` of each
/// filesystem; refused otherwise, naming the variable.
pub fn disk_headroom_of(value: Option<&str>) -> Result<Option<DiskHeadroom>, String> {
    let Some(value) = value.filter(|value| !value.is_empty()) else { return Ok(None) };
    let refused = || format!("CLADEFSD_DISK_HEADROOM is bytes or N%, not {value:?}");
    match value.strip_suffix('%') {
        Some(percent) => match percent.parse() {
            Ok(percent) if percent < 100 => Ok(Some(DiskHeadroom::Percent(percent))),
            _ => Err(refused()),
        },
        None => value.parse().map(|bytes| Some(DiskHeadroom::Bytes(bytes))).map_err(|_| refused()),
    }
}

/// cladefs: what the disk governor leaves free of a filesystem of `size` bytes: `set`, or, unset, a
/// twentieth of it, 4 GiB at least, half of it at most: room for others' writes between its looks,
/// for what its cells' commits add unadmitted (TASK-539.15), and a neighbour's (user, 2026-11-18:
/// 5%, 4 GiB at least).
pub fn disk_headroom(size: u64, set: Option<DiskHeadroom>) -> u64 {
    DiskHeadroom::resolve(size, set)
}

/// cladefs: what the daemon may keep on a disk, holding `held`, its filesystem `free`, `in_flight`
/// of what it holds not on the disk yet, leaving `headroom`: as the memory's allowance, held and
/// free less the headroom.
pub fn disk_allowance(held: u64, free: u64, in_flight: u64, headroom: u64) -> u64 {
    (held + free).saturating_sub(in_flight + headroom)
}

/// cladefs: whether a disk whose cells leave `spare` of its allowance is under pressure, `was` it
/// as the last look found it: entered under `headroom`, left only past it and a quarter more, so
/// that a disk at its edge doesn't urge its uploaders on and off at every look.
pub fn pressured(was: bool, spare: u64, headroom: u64) -> bool {
    match was {
        true => spare < headroom + headroom / 4,
        false => spare < headroom,
    }
}

/// The size of physical memory for a governed daemon: all of the host's, in whole slabs. The
/// host's limit on mappings bounds none of it, as the daemon maps none of it (doc-26).
pub fn memory_bytes() -> Result<u64, String> {
    let host = host_memory().map_err(|err| format!("reading the host's memory: {err}"))?;
    Ok(host.total / SLAB * SLAB)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_gives_total_and_available() {
        let meminfo = "MemTotal:       28311740 kB\nMemFree:          512000 kB\n\
                       MemAvailable:   20000000 kB\nBuffers: 1 kB\n";
        assert_eq!(meminfo_of(meminfo), Some((28311740 * 1024, 20000000 * 1024)));
        assert_eq!(meminfo_of("MemTotal: 1 kB\n"), None);
    }

    #[test]
    fn a_cgroups_room_counts_its_inactive_files_reclaimable() {
        let stat = "anon 100\nfile 900\ninactive_file 300\nactive_file 600\n";
        assert_eq!(
            room_of("8589934592\n", "8000000000\n", stat),
            Some(8589934592 + 300 - 8000000000)
        );
        assert_eq!(room_of("max\n", "8000000000\n", stat), None);
        assert_eq!(room_of("100\n", "1000\n", stat), Some(0));
    }

    /// cladefs: board 5662: the allowance never goes under the floor, however little the host has
    /// left; the host is full under it, and stays full until the room is past it and a quarter more:
    /// the edges a test end to end reaches only by luck.
    #[test]
    fn the_allowance_keeps_its_floor_and_says_the_host_full_under_it() {
        const MIB: u64 = 1 << 20;
        let host = |available| HostMemory { total: 32 << 30, available };
        let floor = 256 * MIB;
        // 40 MiB held, 30 MiB past the headroom available: the room is 70 MiB, the floor kept.
        let squeezed = allowance(40 * MIB, host(1054 * MIB), 1024 * MIB, floor, false);
        assert_eq!(squeezed, Allowance { bytes: floor, full: true });
        // Past the floor, not past it and a quarter more: full still, if it was.
        let between = allowance(40 * MIB, host(1284 * MIB), 1024 * MIB, floor, true);
        assert_eq!(between, Allowance { bytes: 300 * MIB, full: true });
        assert!(!allowance(40 * MIB, host(1284 * MIB), 1024 * MIB, floor, false).full);
        let past = allowance(40 * MIB, host(1324 * MIB), 1024 * MIB, floor, true);
        assert_eq!(past, Allowance { bytes: 340 * MIB, full: false });
    }

    #[test]
    fn disk_headrooms_are_bytes_or_a_share_and_by_default_a_twentieth() {
        const GIB: u64 = 1 << 30;
        assert_eq!(disk_headroom_of(None), Ok(None));
        assert_eq!(disk_headroom_of(Some("")), Ok(None));
        assert_eq!(disk_headroom_of(Some("1024")), Ok(Some(DiskHeadroom::Bytes(1024))));
        assert_eq!(disk_headroom_of(Some("5%")), Ok(Some(DiskHeadroom::Percent(5))));
        for wrong in ["5 %", "100%", "-1", "1G", "%"] {
            assert_eq!(
                disk_headroom_of(Some(wrong)),
                Err(format!("CLADEFSD_DISK_HEADROOM is bytes or N%, not {wrong:?}"))
            );
        }
        // A twentieth of a large disk; 4 GiB at least; half of a small one at most.
        assert_eq!(disk_headroom(4000 * GIB, None), 200 * GIB);
        assert_eq!(disk_headroom(40 * GIB, None), 4 * GIB);
        assert_eq!(disk_headroom(2 * GIB, None), GIB);
        assert_eq!(disk_headroom(4000 * GIB, Some(DiskHeadroom::Percent(2))), 80 * GIB);
        assert_eq!(disk_headroom(4000 * GIB, Some(DiskHeadroom::Bytes(GIB))), GIB);
    }

    #[test]
    fn pressure_enters_under_the_headroom_and_leaves_a_quarter_past_it() {
        let headroom = 400;
        assert!(!pressured(false, 400, headroom));
        assert!(pressured(false, 399, headroom));
        assert!(pressured(true, 499, headroom));
        assert!(!pressured(true, 500, headroom));
    }

    #[test]
    fn shares_go_by_weight() {
        let tenants = [(0xa, 3), (0xb, 1), (0xc, 0)];
        assert_eq!(shares(400, &tenants), [(0xa, 300), (0xb, 100), (0xc, 0)]);
        assert_eq!(shares(400, &[(0xf, 0)]), [(0xf, 0)]);
    }

    /// board 8357: an idle daemon in a limited cgroup on a large host sizes its default headroom
    /// to its cgroup limit (capped at half the limit), not the host's total RAM, so it admits
    /// cells while idle and refuses them only when the cgroup or host is genuinely full.
    #[test]
    fn headroom_in_a_limited_cgroup_sizes_to_the_cgroup_not_the_host() {
        const GIB: u64 = 1 << 30;
        const MIB: u64 = 1 << 20;
        let host_total = 256 * GIB;

        // 8 GiB cgroup on a 256 GiB host: before the fix, headroom was 256/16 = 16 GiB, so
        // `room = (0 + 8 GiB).saturating_sub(16 GiB) = 0 < floor` refused every cell while idle.
        let cgroup_limit = 8 * GIB;
        let headroom = headroom_of(cgroup_limit, None).unwrap();
        let floor = floor_of(cgroup_limit, None).unwrap();
        assert_eq!(headroom, GIB);
        assert_eq!(floor, GIB);

        // Idle cgroup: available is nearly the full 8 GiB cgroup limit -> admits cells.
        let idle = HostMemory { total: host_total, available: cgroup_limit - 64 * MIB };
        let allowed = allowance(0, idle, headroom, floor, false);
        assert!(!allowed.full, "idle 8 GiB cgroup must not be full: {allowed:?}");
        assert!(allowed.bytes > floor, "{allowed:?}");

        // Full cgroup: available drops below headroom + floor -> refuses new cells.
        let full_cgroup = HostMemory { total: host_total, available: headroom + floor - 64 * MIB };
        assert!(allowance(0, full_cgroup, headroom, floor, false).full);

        // Full host: MemAvailable < cgroup room -> refuses new cells.
        let full_host = HostMemory { total: host_total, available: 512 * MIB };
        assert!(allowance(0, full_host, headroom, floor, false).full);

        // Small cgroup (1 GiB): default headroom caps at limit / 2 (512 MiB), floor at limit / 8.
        let small_limit = GIB;
        let small_headroom = headroom_of(small_limit, None).unwrap();
        let small_floor = floor_of(small_limit, None).unwrap();
        assert_eq!(small_headroom, 512 * MIB);
        assert_eq!(small_floor, 128 * MIB);
        let small_idle = HostMemory { total: host_total, available: small_limit - 64 * MIB };
        assert!(!allowance(0, small_idle, small_headroom, small_floor, false).full);

        // Explicit override still takes precedence and rejects non-numeric values.
        assert_eq!(headroom_of(cgroup_limit, Some("4096")), Ok(4096));
        assert!(headroom_of(cgroup_limit, Some("not-a-number")).is_err());
    }
}
