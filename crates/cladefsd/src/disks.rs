// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: daemon-owned JBOD disk pool (`[[disks]]`), placement, cell-ID lookup, and live
//! per-disk governance (hotbed question 547).
//!
//! Callers never pick host directories: `cladefsd` owns every configured disk root and lays out
//! `cells/<TENANT>/<CELL>` and `part-cache/` inside each root automatically.

use bucket_cell::host_disk::{DiskHeadroom, DiskPlacement, DiskPolicy, HostDisk};
use cladefs_core::config::{DiskDefaultsConfig, DiskEntryConfig};
use cladefs_core::{CellId, Errno};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Subdirectory inside each managed disk root holding cell directories (`<TENANT>/<CELL>`).
pub(crate) const CELLS_SUBDIR: &str = "cells";
/// Subdirectory inside each managed disk root holding that disk's part-cache shard.
pub(crate) const PART_CACHE_SUBDIR: &str = "part-cache";

/// One managed local disk inside [`DiskPool`].
#[derive(Debug)]
pub(crate) struct ManagedDisk {
    /// Operator-facing stable disk ID (e.g. `"nvme0"` or `"disk-0"`).
    pub id: String,
    /// `<root>/cells` where cell directories live as `<TENANT_PADDED>/<CELL_PADDED>`.
    pub cells_dir: PathBuf,
    /// `<root>/part-cache` shard directory on the same filesystem (`st_dev`) as `cells_dir`.
    pub part_cache_dir: PathBuf,
    /// Per-filesystem [`HostDisk`] carrying live accounting, allowance, caps, and health.
    pub host_disk: Arc<HostDisk>,
    /// Per-entry policy copy so virtual multi-disk setups sharing one test filesystem still track
    /// distinct per-entry placement/cap governance independently.
    policy: Mutex<DiskPolicy>,
    /// True if another [`ManagedDisk`] in this pool shares the same `st_dev` (single-FS tests).
    shared_fs: bool,
    /// cladefs: its root, locked for the daemon's life: one extent daemon owns a disk, and its
    /// cells, which nothing else opens beside it (the user, 2026-11-26, board 14305).
    #[allow(dead_code, reason = "held for its lock")]
    owned: host_fs::Flock,
}

impl ManagedDisk {
    /// Current placement mode for this managed disk entry.
    pub(crate) fn placement(&self) -> DiskPlacement {
        if self.shared_fs {
            self.policy.lock().unwrap().placement
        } else {
            self.host_disk.placement()
        }
    }

    /// Configured hard cap for this managed disk entry.
    pub(crate) fn cap(&self) -> Option<u64> {
        if self.shared_fs { self.policy.lock().unwrap().cap } else { self.host_disk.cap() }
    }

    /// Configured part-cache cap for this managed disk entry.
    pub(crate) fn cache_cap(&self) -> Option<u64> {
        if self.shared_fs {
            self.policy.lock().unwrap().cache_cap
        } else {
            self.host_disk.cache_cap()
        }
    }

    /// Configured dirty-page budget for this managed disk entry.
    pub(crate) fn dirty_budget(&self) -> Option<u64> {
        if self.shared_fs {
            self.policy.lock().unwrap().dirty_budget
        } else {
            self.host_disk.dirty_budget()
        }
    }

    /// Whether new cells may be placed on this managed disk entry.
    pub(crate) fn accepts_new_cells(&self) -> bool {
        self.host_disk.health() == bucket_cell::host_disk::DiskHealthState::Online
            && self.placement() == DiskPlacement::ReadWrite
    }

    /// Spare bytes available for placing new cells on this managed disk entry.
    pub(crate) fn spare(&self) -> u64 {
        let allowance = self.host_disk.allowance();
        let effective = match self.cap() {
            Some(cap) => allowance.min(cap),
            None => allowance,
        };
        effective.saturating_sub(self.host_disk.cells.committed())
    }
}

/// Live runtime governance update applied via `SetDiskGovernance`.
#[derive(Clone, Debug, Default)]
pub(crate) struct GovernanceUpdate {
    /// `Some(Some(bytes))` sets a cap, `Some(None)` clears it, `None` leaves unchanged.
    pub cap: Option<Option<u64>>,
    /// `Some(Some(bytes))` sets a part-cache cap, `Some(None)` clears it, `None` leaves unchanged.
    pub cache_cap: Option<Option<u64>>,
    /// `Some(Some(headroom))` sets a headroom override, `Some(None)` restores default.
    pub headroom: Option<Option<DiskHeadroom>>,
    /// `Some(Some(bytes))` sets a per-disk dirty budget, `Some(None)` restores auto-division.
    pub dirty_budget: Option<Option<u64>>,
    /// `Some(placement)` changes placement mode (`ReadWrite`, `Drain`, `ReadOnly`).
    pub placement: Option<DiskPlacement>,
}

/// RAII guard reserving a caller-named [`CellId`] across concurrent `CreateCell(cell != 0)` calls
/// so retries cannot race on `find_cell`, `removed_if_unmade_on`, or `pick_disk` across disks.
pub(crate) struct CellReservation {
    pool: Arc<DiskPool>,
    cell: CellId,
}

impl Drop for CellReservation {
    fn drop(&mut self) {
        self.pool
            .inflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.cell);
    }
}

/// cladefs: the disk at `root` owned by this daemon: its root locked, exclusively, as long as the
/// daemon runs; refused while another process owns it, another extent daemon, which a host has
/// one of (board 14305).
fn own(root: &std::path::Path) -> Result<host_fs::Flock, String> {
    let dir = host_fs::File::open(root).map_err(|err| format!("opening it: {err}"))?;
    host_fs::Flock::try_lock(dir).map_err(|err| match err.kind() {
        std::io::ErrorKind::WouldBlock => {
            "another extent daemon owns it: a host has one, which its cells are opened through"
                .to_owned()
        }
        _ => format!("locking it: {err}"),
    })
}

/// The daemon's managed pool of local disks and cell-ID-to-path index.
pub(crate) struct DiskPool {
    disks: Vec<Arc<ManagedDisk>>,
    /// Cached mapping from `CellId` to its canonical on-disk cell directory across the pool.
    index: Mutex<HashMap<CellId, PathBuf>>,
    /// Caller-named cells currently being created/taken across the pool.
    inflight: Mutex<HashSet<CellId>>,
}

impl DiskPool {
    /// Opens and validates all configured `[[disks]]`, creating `cells/` and (when requested)
    /// `part-cache/` inside each root and applying each disk's [`DiskPolicy`].
    pub(crate) fn open(
        host: &bucket_cell::Host,
        entries: &[DiskEntryConfig],
        defaults: &DiskDefaultsConfig,
        enable_part_cache: bool,
    ) -> Result<Self, String> {
        if entries.is_empty() {
            return Err("DiskPool requires at least one configured disk".to_owned());
        }
        let mut dev_counts: HashMap<u64, usize> = HashMap::new();
        let mut roots: HashMap<PathBuf, String> = HashMap::new();
        let mut raw_disks = Vec::with_capacity(entries.len());
        for (i, entry) in entries.iter().enumerate() {
            let id = entry.effective_id(i);
            host_fs::create_dir_all(&entry.path)
                .map_err(|err| format!("disk {id} ({}): {err}", entry.path.display()))?;
            let root = host_fs::canonicalize(&entry.path)
                .map_err(|err| format!("disk {id} ({}): {err}", entry.path.display()))?;
            // cladefs: one directory named twice, as by a symlink, is refused as such, not taken
            // for another daemon's, its first entry's lock refusing its second (review 170).
            if let Some(other) = roots.insert(root.clone(), id.clone()) {
                return Err(format!(
                    "disk {id} ({}): the directory of disk {other}",
                    root.display()
                ));
            }
            let owned =
                own(&root).map_err(|err| format!("disk {id} ({}): {err}", root.display()))?;
            let cells_dir = root.join(CELLS_SUBDIR);
            let part_cache_dir = root.join(PART_CACHE_SUBDIR);
            host_fs::create_dir_all(&cells_dir)
                .map_err(|err| format!("disk {id} ({}): {err}", cells_dir.display()))?;
            if enable_part_cache {
                host_fs::create_dir_all(&part_cache_dir)
                    .map_err(|err| format!("disk {id} ({}): {err}", part_cache_dir.display()))?;
            }
            let host_disk = HostDisk::of_on(host, &cells_dir)
                .map_err(|err| format!("disk {id} ({}): {err}", cells_dir.display()))?;
            if let Some(dev) = host_disk.dev() {
                let count = dev_counts.entry(dev).or_insert(0);
                *count += 1;
                if *count > 1 {
                    log::warn!(
                        "cladefsd: disk {id} ({}) shares filesystem dev {dev:#x} with another disk",
                        root.display()
                    );
                }
            }
            let policy = entry.to_policy(i, defaults);
            raw_disks.push((id, cells_dir, part_cache_dir, host_disk, policy, owned));
        }
        let mut disks = Vec::with_capacity(raw_disks.len());
        let mut shared_devs: HashSet<u64> = HashSet::new();
        for (id, cells_dir, part_cache_dir, host_disk, policy, owned) in raw_disks {
            let shared_fs =
                host_disk.dev().is_some_and(|dev| dev_counts.get(&dev).copied().unwrap_or(0) > 1);
            if !shared_fs {
                host_disk.set_policy(policy.clone());
            } else if let Some(dev) = host_disk.dev() {
                shared_devs.insert(dev);
            }
            disks.push(Arc::new(ManagedDisk {
                id,
                cells_dir,
                part_cache_dir,
                host_disk,
                policy: Mutex::new(policy),
                shared_fs,
                owned,
            }));
        }
        let pool =
            Self { disks, index: Mutex::new(HashMap::new()), inflight: Mutex::new(HashSet::new()) };
        for dev in shared_devs {
            pool.sync_shared_host_disk(dev);
        }
        pool.scan_existing();
        Ok(pool)
    }

    /// Creates a [`DiskPool`] from legacy root directories (for tests or env-var boot).
    pub(crate) fn from_roots(host: &bucket_cell::Host, roots: &[PathBuf]) -> Result<Self, String> {
        let entries: Vec<DiskEntryConfig> = roots
            .iter()
            .enumerate()
            .map(|(i, path)| DiskEntryConfig {
                id: Some(format!("disk-{i}")),
                path: path.clone(),
                cap: None,
                headroom: None,
                cache_cap: None,
                dirty_budget: None,
                placement: cladefs_core::config::PlacementSpec::ReadWrite,
            })
            .collect();
        Self::open(host, &entries, &DiskDefaultsConfig::default(), false)
    }

    /// Scans `<disk>/cells/<TENANT>/<CELL>` on boot to warm the in-memory cell index.
    fn scan_existing(&self) {
        let mut index = self.index.lock().unwrap();
        for disk in &self.disks {
            let Ok(tenants) = host_fs::read_dir(&disk.cells_dir) else { continue };
            for tenant_entry in tenants.flatten() {
                let tenant_name = tenant_entry.file_name();
                let Some(tenant_str) = tenant_name.to_str() else { continue };
                let Some(tenant) = base32_id::parse_padded(tenant_str) else { continue };
                let tenant_dir = tenant_entry.path();
                let Ok(cells) = host_fs::read_dir(&tenant_dir) else { continue };
                for cell_entry in cells.flatten() {
                    let cell_name = cell_entry.file_name();
                    let Some(cell_str) = cell_name.to_str() else { continue };
                    let Some(id) = base32_id::parse_padded(cell_str) else { continue };
                    let path = cell_entry.path();
                    if host_fs::is_dir(&path) {
                        let cell_id = CellId { tenant, id };
                        if let Some(prev) = index.get(&cell_id) {
                            let prev_unmade = host_fs::exists(prev.join("unmade"));
                            let next_unmade = host_fs::exists(path.join("unmade"));
                            log::warn!(
                                "cladefsd: duplicate cell {cell_id} found at {} and {}",
                                prev.display(),
                                path.display()
                            );
                            if prev_unmade && !next_unmade {
                                index.insert(cell_id, path);
                            }
                        } else {
                            index.insert(cell_id, path);
                        }
                    }
                }
            }
        }
    }

    /// All managed disks in configuration order.
    pub(crate) fn disks(&self) -> &[Arc<ManagedDisk>] {
        &self.disks
    }

    /// All `<disk>/cells` root directories.
    pub(crate) fn cell_roots(&self) -> Vec<PathBuf> {
        self.disks.iter().map(|d| d.cells_dir.clone()).collect()
    }

    /// All `<disk>/part-cache` directories.
    pub(crate) fn part_cache_dirs(&self) -> Vec<PathBuf> {
        self.disks.iter().map(|d| d.part_cache_dir.clone()).collect()
    }

    /// Picks the best managed disk for placing a new cell:
    /// 1. Only disks that are `Online` and `ReadWrite` (`accepts_new_cells()`).
    /// 2. Strongly prefers disks not under pressure.
    /// 3. Among candidates, picks the disk with the most `spare` bytes (ties broken by config order).
    /// Returns `ENOSPC` if no disk is eligible or every eligible disk has zero spare space.
    pub(crate) fn pick_disk(&self) -> Result<Arc<ManagedDisk>, Errno> {
        let mut best_healthy: Option<(u64, &Arc<ManagedDisk>)> = None;
        let mut best_pressured: Option<(u64, &Arc<ManagedDisk>)> = None;
        for disk in &self.disks {
            if !disk.accepts_new_cells() {
                continue;
            }
            let spare = disk.spare();
            if spare == 0 {
                continue;
            }
            if !disk.host_disk.under_pressure() {
                if best_healthy.is_none_or(|(best_spare, _)| spare > best_spare) {
                    best_healthy = Some((spare, disk));
                }
            } else if best_pressured.is_none_or(|(best_spare, _)| spare > best_spare) {
                best_pressured = Some((spare, disk));
            }
        }
        best_healthy.or(best_pressured).map(|(_, d)| Arc::clone(d)).ok_or(Errno::ENOSPC)
    }

    /// Canonical path where `cell` lives on `disk`: `<disk.cells_dir>/<TENANT_PADDED>/<CELL_PADDED>`.
    pub(crate) fn cell_path(disk: &ManagedDisk, cell: CellId) -> PathBuf {
        disk.cells_dir
            .join(base32_id::Padded(cell.tenant).to_string())
            .join(base32_id::Padded(cell.id).to_string())
    }

    /// Ensures `<disk.cells_dir>/<TENANT_PADDED>` exists durably on `disk` and returns it.
    pub(crate) fn ensure_tenant_dir(disk: &ManagedDisk, tenant: u64) -> Result<PathBuf, Errno> {
        let dir = disk.cells_dir.join(base32_id::Padded(tenant).to_string());
        host_fs::create_dir_all_durably(&dir).map_err(|err| {
            log::warn!("cladefsd: creating tenant dir {}: {err}", dir.display());
            Errno::EIO
        })?;
        Ok(dir)
    }

    /// Ensures `<disk.cells_dir>/<TENANT_PADDED>` exists on every managed disk and returns them all
    /// (for `Grant` per-disk allotments). Stable across `Drain`/`ReadOnly` placement changes so
    /// re-granting remains idempotent.
    pub(crate) fn ensure_tenant_dirs(&self, tenant: u64) -> Result<Vec<PathBuf>, Errno> {
        let mut dirs = Vec::with_capacity(self.disks.len());
        for disk in &self.disks {
            let dir = disk.cells_dir.join(base32_id::Padded(tenant).to_string());
            if host_fs::is_dir(&dir) {
                dirs.push(dir);
            } else if let Ok(created) = Self::ensure_tenant_dir(disk, tenant) {
                dirs.push(created);
            }
        }
        if dirs.is_empty() {
            return Err(Errno::ENOSPC);
        }
        Ok(dirs)
    }

    /// Returns a unique staging path inside `<disk.cells_dir>/<TENANT_PADDED>/.staging-<rand>`
    /// (not yet created on disk) so a cell being created/forked/imported is attributed to `tenant`'s
    /// allotment on `disk` immediately and renames atomically into `<CELL_PADDED>` on the same FS.
    pub(crate) fn staging_path(disk: &ManagedDisk, tenant: u64) -> Result<PathBuf, Errno> {
        let tenant_dir = Self::ensure_tenant_dir(disk, tenant)?;
        let nonce: u64 = rand::random();
        Ok(tenant_dir.join(format!(".staging-{nonce:016x}")))
    }

    /// Atomically moves a newly constructed cell from `staging` into its canonical `<TENANT>/<CELL>`
    /// path on `disk`, syncs the parent tenant directory so the rename is durable before any RPC
    /// acknowledges the cell, registers it in the index, and returns the final path.
    pub(crate) fn commit_staged(
        &self,
        disk: &ManagedDisk,
        cell: CellId,
        staging: &Path,
    ) -> Result<PathBuf, Errno> {
        let tenant_dir = Self::ensure_tenant_dir(disk, cell.tenant)?;
        let final_path = Self::cell_path(disk, cell);
        host_fs::rename(staging, &final_path).map_err(|err| {
            log::warn!(
                "cladefsd: renaming staged cell {} -> {}: {err}",
                staging.display(),
                final_path.display()
            );
            Errno::EIO
        })?;
        host_fs::sync_dir(&tenant_dir).map_err(|err| {
            log::warn!("cladefsd: syncing tenant dir {}: {err}", tenant_dir.display());
            Errno::EIO
        })?;
        self.register_cell(cell, final_path.clone());
        Ok(final_path)
    }

    /// Reserves a caller-named `cell` against concurrent `CreateCell(cell != 0)` calls so retries
    /// receive `EEXIST` instead of racing on `find_cell`, `removed_if_unmade_on`, or `pick_disk`.
    pub(crate) fn reserve_cell(self: &Arc<Self>, cell: CellId) -> Result<CellReservation, Errno> {
        let mut inflight = self.inflight.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !inflight.insert(cell) {
            return Err(Errno::EEXIST);
        }
        Ok(CellReservation { pool: Arc::clone(self), cell })
    }

    /// Finds the host directory of `cell` across the disk pool, checking the in-memory index first
    /// and falling back to probing `<disk.cells_dir>/<TENANT_PADDED>/<CELL_PADDED>`.
    pub(crate) fn find_cell(&self, cell: CellId) -> Option<PathBuf> {
        let cached = self.index.lock().unwrap().get(&cell).cloned();
        if let Some(cached) = cached {
            if host_fs::exists(&cached) {
                return Some(cached);
            }
            self.index.lock().unwrap().remove(&cell);
        }
        for disk in &self.disks {
            let candidate = Self::cell_path(disk, cell);
            if host_fs::exists(&candidate) {
                self.index.lock().unwrap().insert(cell, candidate.clone());
                return Some(candidate);
            }
        }
        None
    }

    /// Records `cell` at `path` in the in-memory index.
    pub(crate) fn register_cell(&self, cell: CellId, path: PathBuf) {
        self.index.lock().unwrap().insert(cell, path);
    }

    /// Removes `cell` from the in-memory index (e.g. after delete, drop, or migration hand-over).
    pub(crate) fn forget_cell(&self, cell: CellId) {
        self.index.lock().unwrap().remove(&cell);
    }

    /// Returns the managed disk ID whose `cells_dir` contains `path`, if any.
    pub(crate) fn disk_id_for_path(&self, path: &Path) -> Option<String> {
        self.disks.iter().find(|d| path.starts_with(&d.cells_dir)).map(|d| d.id.clone())
    }

    /// Looks up a managed disk by its configured ID.
    pub(crate) fn by_id(&self, id: &str) -> Option<Arc<ManagedDisk>> {
        self.disks.iter().find(|d| d.id == id).cloned()
    }

    /// Applies a live runtime governance update to the disk identified by `disk_id`.
    pub(crate) fn update_governance(
        &self,
        disk_id: &str,
        update: GovernanceUpdate,
    ) -> Result<(), Errno> {
        let disk = self.by_id(disk_id).ok_or(Errno::ENOENT)?;
        if let Some(Some(0)) = update.cap {
            return Err(Errno::EINVAL);
        }
        if let Some(Some(0)) = update.cache_cap {
            return Err(Errno::EINVAL);
        }
        if let Some(Some(DiskHeadroom::Percent(p))) = update.headroom
            && p >= 100
        {
            return Err(Errno::EINVAL);
        }
        if let Some(Some(0)) = update.dirty_budget {
            return Err(Errno::EINVAL);
        }
        {
            let mut policy = disk.policy.lock().unwrap();
            if let Some(cap) = update.cap {
                policy.cap = cap;
            }
            if let Some(cache_cap) = update.cache_cap {
                policy.cache_cap = cache_cap;
            }
            if let Some(headroom) = update.headroom {
                policy.headroom = headroom;
            }
            if let Some(dirty_budget) = update.dirty_budget {
                policy.dirty_budget = dirty_budget;
            }
            if let Some(placement) = update.placement {
                policy.placement = placement;
            }
        }
        if !disk.shared_fs {
            if let Some(cap) = update.cap {
                disk.host_disk.set_cap(cap);
            }
            if let Some(cache_cap) = update.cache_cap {
                disk.host_disk.set_cache_cap(cache_cap);
            }
            if let Some(headroom) = update.headroom {
                disk.host_disk.set_policy_headroom(headroom);
            }
            if let Some(dirty_budget) = update.dirty_budget {
                disk.host_disk.set_dirty_budget(dirty_budget);
            }
            if let Some(placement) = update.placement {
                disk.host_disk.set_placement(placement);
            }
        } else if let Some(dev) = disk.host_disk.dev() {
            self.sync_shared_host_disk(dev);
        }
        Ok(())
    }

    /// Recomputes and applies an order-independent aggregate [`DiskPolicy`] on the shared
    /// [`HostDisk`] backing all [`ManagedDisk`] entries on filesystem `dev`:
    /// - `cap`, `cache_cap`, `dirty_budget`: strictest (minimum) configured limit across siblings so
    ///   an uncapped sibling never silently disables another sibling's cap.
    /// - `headroom`: strictest (largest resolved bytes) headroom override across siblings.
    /// - `placement`: `ReadOnly` iff all siblings are `ReadOnly`, else `Drain` iff all are
    ///   `Drain`/`ReadOnly`, else `ReadWrite`.
    fn sync_shared_host_disk(&self, dev: u64) {
        let siblings: Vec<&Arc<ManagedDisk>> =
            self.disks.iter().filter(|d| d.host_disk.dev() == Some(dev)).collect();
        let Some(first) = siblings.first() else { return };
        // Sample disk size once to compare mixed Bytes/Percent headroom overrides consistently;
        // falls back to 0 when statvfs is unavailable.
        let disk_size = first.host_disk.sample().map(|s| s.size).unwrap_or(0);
        let mut cap: Option<u64> = None;
        let mut cache_cap: Option<u64> = None;
        let mut dirty_budget: Option<u64> = None;
        let mut headroom: Option<DiskHeadroom> = None;
        let mut all_ro = true;
        let mut all_no_new = true;
        for d in &siblings {
            let p = d.policy.lock().unwrap();
            if let Some(c) = p.cap {
                cap = Some(cap.map_or(c, |cur| cur.min(c)));
            }
            if let Some(c) = p.cache_cap {
                cache_cap = Some(cache_cap.map_or(c, |cur| cur.min(c)));
            }
            if let Some(b) = p.dirty_budget {
                dirty_budget = Some(dirty_budget.map_or(b, |cur| cur.min(b)));
            }
            if let Some(h) = p.headroom {
                headroom = Some(match headroom {
                    None => h,
                    Some(cur)
                        if DiskHeadroom::resolve(disk_size, Some(h))
                            > DiskHeadroom::resolve(disk_size, Some(cur)) =>
                    {
                        h
                    }
                    Some(cur) => cur,
                });
            }
            if p.placement != DiskPlacement::ReadOnly {
                all_ro = false;
            }
            if p.placement == DiskPlacement::ReadWrite {
                all_no_new = false;
            }
        }
        let placement = if all_ro {
            DiskPlacement::ReadOnly
        } else if all_no_new {
            DiskPlacement::Drain
        } else {
            DiskPlacement::ReadWrite
        };
        let mut merged = DiskPolicy::default();
        merged.cap = cap;
        merged.cache_cap = cache_cap;
        merged.dirty_budget = dirty_budget;
        merged.headroom = headroom;
        merged.placement = placement;
        first.host_disk.set_policy(merged);
    }

    /// Removes any leftover `.staging-*` directories left by a prior crash across all disks.
    pub(crate) fn reap_staging(&self) -> usize {
        let mut removed = 0;
        for disk in &self.disks {
            let Ok(tenants) = host_fs::read_dir(&disk.cells_dir) else { continue };
            for tenant_entry in tenants.flatten() {
                let Ok(entries) = host_fs::read_dir(tenant_entry.path()) else { continue };
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    if name.to_str().is_some_and(|s| s.starts_with(".staging-")) {
                        if let Err(err) = host_fs::remove_dir_all(entry.path()) {
                            log::warn!(
                                "cladefsd: reaping staging {}: {err}",
                                entry.path().display()
                            );
                        } else {
                            removed += 1;
                        }
                    }
                }
            }
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cladefs_core::config::{ByteSize, PlacementSpec};

    fn entry(id: &str, path: PathBuf, cap: Option<u64>) -> DiskEntryConfig {
        DiskEntryConfig {
            id: Some(id.to_owned()),
            path,
            cap: cap.map(ByteSize),
            headroom: None,
            cache_cap: None,
            dirty_budget: None,
            placement: PlacementSpec::ReadWrite,
        }
    }

    /// A disk is one daemon's at a time: its root, locked as the daemon's pool opens it, refuses
    /// another's pool while the first lives, and is taken by one once the first is gone. What
    /// holds a cell's directory from other processes since each cell's own lock went (board
    /// 14305), as a test of a cell directory's lock had checked of an uploader's hold, until it
    /// checked what nothing did any more (TASK-129, board 14471).
    #[test]
    fn disks_are_owned_by_one_daemon_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("disk0");
        let open = || DiskPool::from_roots(&bucket_cell::Host::new(), std::slice::from_ref(&root));
        let first = open().unwrap();
        let canonical = root.canonicalize().unwrap();
        assert_eq!(
            open().err(),
            Some(format!(
                "disk disk-0 ({}): another extent daemon owns it: a host has one, which its cells \
                 are opened through",
                canonical.display()
            ))
        );
        drop(first);
        assert_eq!(open().err(), None, "taken once its owner is gone");
    }

    #[test]
    fn find_cell_evicts_vanished_cached_path_without_deadlocking() {
        let dir = tempfile::tempdir().unwrap();
        let host = bucket_cell::Host::new();
        let pool = Arc::new(DiskPool::from_roots(&host, &[dir.path().join("disk0")]).unwrap());
        let cell = CellId { tenant: 1, id: 42 };
        let path = DiskPool::cell_path(&pool.disks()[0], cell);
        std::fs::create_dir_all(&path).unwrap();
        pool.register_cell(cell, path.clone());
        std::fs::remove_dir_all(&path).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let pool_t = Arc::clone(&pool);
        std::thread::spawn(move || {
            let found = pool_t.find_cell(cell);
            let _ = tx.send(found);
        });
        let found = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("find_cell deadlocked on vanished cached path");
        assert_eq!(found, None);
        assert_eq!(pool.find_cell(CellId { tenant: 1, id: 43 }), None);
    }

    #[test]
    fn shared_fs_disks_aggregate_strictest_caps_order_independently() {
        let dir = tempfile::tempdir().unwrap();
        let cap_40g = 40u64 << 30;
        let cap_20g = 20u64 << 30;
        let defaults = DiskDefaultsConfig::default();

        let host_ab = bucket_cell::Host::new();
        let pool_ab = DiskPool::open(
            &host_ab,
            &[
                entry("a", dir.path().join("a1"), Some(cap_40g)),
                entry("b", dir.path().join("b1"), None),
            ],
            &defaults,
            false,
        )
        .unwrap();
        assert_eq!(pool_ab.disks()[0].host_disk.cap(), Some(cap_40g));

        let host_ba = bucket_cell::Host::new();
        let pool_ba = DiskPool::open(
            &host_ba,
            &[
                entry("b", dir.path().join("b2"), None),
                entry("a", dir.path().join("a2"), Some(cap_40g)),
            ],
            &defaults,
            false,
        )
        .unwrap();
        assert_eq!(pool_ba.disks()[0].host_disk.cap(), Some(cap_40g));

        // Live governance tightening and clearing propagates to the shared HostDisk.
        pool_ab
            .update_governance(
                "b",
                GovernanceUpdate { cap: Some(Some(cap_20g)), ..Default::default() },
            )
            .unwrap();
        assert_eq!(pool_ab.disks()[0].host_disk.cap(), Some(cap_20g));
        pool_ab
            .update_governance("b", GovernanceUpdate { cap: Some(None), ..Default::default() })
            .unwrap();
        assert_eq!(pool_ab.disks()[0].host_disk.cap(), Some(cap_40g));
        pool_ab
            .update_governance("a", GovernanceUpdate { cap: Some(None), ..Default::default() })
            .unwrap();
        assert_eq!(pool_ab.disks()[0].host_disk.cap(), None);
    }

    #[test]
    fn reserve_cell_and_commit_staged_enforce_uniqueness_and_reap_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let host = bucket_cell::Host::new();
        let pool = Arc::new(DiskPool::from_roots(&host, &[dir.path().join("disk0")]).unwrap());
        let cell = CellId { tenant: 2, id: 9 };
        let guard = pool.reserve_cell(cell).unwrap();
        assert_eq!(pool.reserve_cell(cell).err(), Some(Errno::EEXIST));
        drop(guard);
        let _guard2 = pool.reserve_cell(cell).unwrap();

        let disk = &pool.disks()[0];
        let staging = DiskPool::staging_path(disk, cell.tenant).unwrap();
        std::fs::create_dir_all(&staging).unwrap();
        let final_path = pool.commit_staged(disk, cell, &staging).unwrap();
        assert!(final_path.is_dir());

        let leftover = DiskPool::staging_path(disk, cell.tenant).unwrap();
        std::fs::create_dir_all(&leftover).unwrap();
        assert_eq!(pool.reap_staging(), 1);
        assert!(!leftover.exists());
        assert!(final_path.is_dir());
    }
}
