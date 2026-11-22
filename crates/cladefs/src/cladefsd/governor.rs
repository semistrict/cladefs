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

use super::memory;
use physmem::CHUNK_SIZE;
use std::time::Duration;

/// How often the governor looks at the host's memory.
pub const INTERVAL: Duration = Duration::from_millis(100);

/// What the governor leaves free of the memory available to the daemon:
/// `CLADEFSD_HEADROOM` bytes, or a sixteenth of the host's memory, 1 GiB at least: room for
/// VMs' memory to grow into between looks, and for the host's own.
pub fn headroom(total: u64) -> Result<u64, String> {
    match node_local::env::var("CLADEFSD_HEADROOM") {
        Ok(bytes) if !bytes.is_empty() => {
            bytes.parse().map_err(|_| format!("CLADEFSD_HEADROOM is bytes, not {bytes:?}"))
        }
        _ => Ok((total / 16).max(1 << 30)),
    }
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
    let page = physmem::page_size();
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
    let max: u64 = max.trim().parse().ok()?;
    let current: u64 = current.trim().parse().ok()?;
    let inactive: u64 = stat
        .lines()
        .find_map(|line| line.strip_prefix("inactive_file "))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    Some((max + inactive).saturating_sub(current))
}

/// What the daemon may hold, holding `held`, of `host`, leaving `headroom`: whole chunks, one at
/// least.
pub fn allowance(held: u64, host: HostMemory, headroom: u64) -> u64 {
    let chunk = CHUNK_SIZE as u64;
    ((held + host.available).saturating_sub(headroom) / chunk * chunk).max(chunk)
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

/// Starts the governor, on a thread of its own, for the rest of the daemon's life. Fails if the
/// host's memory can't be read, or the headroom is malformed: a daemon that can't see the host's
/// memory doesn't start rather than run without its cache. cladefs: a service of its own
/// ([`super::run::service`]), one worker's runtime on its thread, as its shrinks wait for the pages
/// they evict holding no thread (board 2946).
pub fn start() -> Result<(), String> {
    let host = host_memory().map_err(|err| format!("reading the host's memory: {err}"))?;
    let headroom = headroom(host.total)?;
    super::run::service("memory governor", 1, move || govern_every(headroom))
        .map_err(|err| format!("starting the memory governor: {err}"))
}

/// cladefs: governs the daemon's memory now and every [`INTERVAL`] after, leaving `headroom` free
/// ([`start`]), for as long as the daemon runs.
async fn govern_every(headroom: u64) {
    loop {
        match host_memory() {
            Ok(host) => memory::govern(allowance(memory::governed_bytes(), host, headroom)).await,
            Err(err) => log::error!("cladefsd: reading the host's memory: {err}"),
        }
        tokio::time::sleep(INTERVAL).await;
    }
}

/// cladefs: what the disk governor leaves free of a filesystem, as `CLADEFSD_DISK_HEADROOM` says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskHeadroom {
    /// So many bytes.
    Bytes(u64),
    /// So many hundredths of the filesystem.
    Percent(u64),
}

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
    match set {
        Some(DiskHeadroom::Bytes(bytes)) => bytes,
        Some(DiskHeadroom::Percent(percent)) => size / 100 * percent,
        None => (size / 20).max(4 << 30).min(size / 2),
    }
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

/// The size of physical memory for a governed daemon: all of the host's, in whole chunks. Fails if
/// the host maps less (`vm.max_map_count` on Linux): a daemon whose cache can't grow into the
/// host's memory doesn't start.
pub fn memory_bytes() -> Result<u64, String> {
    let host = host_memory().map_err(|err| format!("reading the host's memory: {err}"))?;
    let chunk = CHUNK_SIZE as u64;
    let bytes = host.total / chunk * chunk;
    if let Some(max) = physmem::max_size().filter(|&max| bytes > max) {
        return Err(format!(
            "the host's memory, {bytes} bytes, is more than the daemon can map, {max}: raise \
             vm.max_map_count"
        ));
    }
    Ok(bytes)
}
