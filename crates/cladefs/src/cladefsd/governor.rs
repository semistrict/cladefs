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

use super::{Daemon, memory};
use crate::slabs::SLAB;
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
/// `CLADEFSD_MEMORY_FLOOR` bytes, or a sixty-fourth of the memory the daemon may have, `limit`
/// ([`limit`]), 256 MiB at least and an eighth of it at most, in whole slabs: room for what the
/// VMs' windows use, their guests' DAX faults' pages (board 5662; 176 small VMs on one base used
/// about 40 MiB of it). Of the cgroup's limit, not the host's, where that is less: a floor past
/// what the cgroup holds would have its OOM killer end what relief would have shrunk.
pub fn floor_of(limit: u64, set: Option<&str>) -> Result<u64, String> {
    let bytes = match set {
        Some(bytes) if !bytes.is_empty() => {
            bytes.parse().map_err(|_| format!("CLADEFSD_MEMORY_FLOOR is bytes, not {bytes:?}"))?
        }
        _ => (limit / 64).max(256 << 20).min(limit / 8),
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

/// cladefs: the memory governor's state, owned by [`Daemon`].
#[derive(Default)]
pub(super) struct State {
    full: AtomicBool,
    pressured: AtomicBool,
    fulls: AtomicU64,
    floor: AtomicU64,
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

/// cladefs: what the governor last found and did, for the daemon's metrics (board 8565): whether
/// the host is full ([`full`]); whether the daemon held past what it may, evicting
/// ([`Governed::pressured`]); how often the host became full; and the floor it keeps, 0 for a
/// daemon not governed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Governed {
    /// Whether the host is full.
    pub full: bool,
    /// Whether the daemon held past what it may at the governor's last look, evicting.
    pub pressured: bool,
    /// How often the host became full.
    pub fulls: u64,
    /// The floor the governor keeps the daemon's memory at, 0 for a daemon not governed.
    pub floor: u64,
}

/// cladefs: what `daemon`'s governor last found and did ([`Governed`]).
pub(super) fn governed_on(daemon: &Daemon) -> Governed {
    Governed {
        full: full_on(daemon),
        pressured: daemon.governor.pressured.load(Ordering::Relaxed),
        fulls: daemon.governor.fulls.load(Ordering::Relaxed),
        floor: daemon.governor.floor.load(Ordering::Relaxed),
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

/// The host's memory now (Linux: /proc/meminfo, and the daemon's cgroups').
#[cfg(target_os = "linux")]
pub fn host_memory() -> std::io::Result<HostMemory> {
    let meminfo = std::fs::read_to_string("/proc/meminfo")?;
    let (total, available) = meminfo_of(&meminfo).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "/proc/meminfo: no MemTotal, MemAvailable",
        )
    })?;
    let cgroups = std::fs::read_to_string("/proc/self/cgroup")?;
    let mut available = available;
    if let Some(path) = cgroups.lines().find_map(|line| line.strip_prefix("0::")) {
        let mut dir = std::path::Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
        loop {
            if let Some(room) = cgroup_room(&dir)? {
                available = available.min(room);
            }
            if !dir.pop() || dir == std::path::Path::new("/sys/fs") {
                break;
            }
        }
    }
    Ok(HostMemory { total, available })
}

/// The host's memory now (macOS: its pages free and speculative, as the VM statistics count them).
#[cfg(target_os = "macos")]
pub fn host_memory() -> std::io::Result<HostMemory> {
    let page = crate::slabs::page_size();
    let total = sysctl_u64(c"hw.memsize")?;
    let free = sysctl_u64(c"vm.page_free_count")? + sysctl_u64(c"vm.page_speculative_count")?;
    Ok(HostMemory { total, available: free * page })
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

/// Starts `daemon`'s memory governor on its runtime for the rest of the daemon's life. Fails if
/// the host's memory can't be read, or the headroom is malformed: a daemon that can't see the
/// host's memory doesn't start rather than run without its cache. cladefs: a supervised service on
/// the daemon's runtime ([`super::run::service`]), as its shrinks wait for the pages they evict
/// holding no thread (board 2946).
pub(super) fn start_on(daemon: &Arc<Daemon>) -> Result<(), String> {
    let host = host_memory().map_err(|err| format!("reading the host's memory: {err}"))?;
    let limit = limit(host.total);
    let (headroom, floor) = (headroom(limit)?, floor(limit)?);
    daemon.governor.floor.store(floor, Ordering::Relaxed);
    let governing = Arc::clone(daemon);
    super::run::service(daemon, "memory governor", move || govern_every(governing, headroom, floor))
        .map_err(|err| format!("starting the memory governor: {err}"))
}

/// Starts the process's default daemon's memory governor.
#[cfg(not(madsim))]
pub fn start() -> Result<(), String> {
    start_on(Daemon::process())
}

/// cladefs: governs `daemon`'s memory now and every [`INTERVAL`] after, leaving `headroom` free
/// and keeping `floor` ([`start_on`]), for as long as the daemon runs.
async fn govern_every(daemon: Arc<Daemon>, headroom: u64, floor: u64) {
    loop {
        match host_memory() {
            Ok(host) => {
                let was = daemon.governor.full.load(Ordering::Relaxed);
                let held = memory::governed_bytes_on(&daemon);
                let allowed = allowance(held, host, headroom, floor, was);
                daemon.governor.pressured.store(held > allowed.bytes, Ordering::Relaxed);
                if allowed.full && !was {
                    daemon.governor.fulls.fetch_add(1, Ordering::Relaxed);
                }
                if allowed.full != was {
                    log::warn!(
                        "cladefsd: the host's memory is {}: {} bytes available, the floor {floor}",
                        if allowed.full { "full, making no cell more" } else { "no longer full" },
                        host.available
                    );
                }
                daemon.governor.full.store(allowed.full, Ordering::Relaxed);
                memory::govern_on(&daemon, allowed.bytes).await
            }
            Err(err) => log::error!("cladefsd: reading the host's memory: {err}"),
        }
        tokio::time::sleep(INTERVAL).await;
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
        assert_eq!(floor, 256 * MIB);

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

        // Small cgroup (1 GiB): default headroom caps at limit / 2 (512 MiB), not 1 GiB.
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
