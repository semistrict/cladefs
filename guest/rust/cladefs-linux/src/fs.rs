// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Fxfs on an extent-store device (`Filesystem`), and its volumes as the kernel mounts them, the
//! one cladefs's images keep their files in by default, each served by object ID, which is the
//! kernel's inode number within the volume, through Fxfs's POSIX layer (crates/fxfs-posix), whose
//! VFS is the kernel's (`Guest`).
//!
//! cladefs: what only the guest has is here: its device's DAX, uploaded commits, resizes and
//! freezes.

use crate::device::{
    Dax, DeviceStats, ExtentDevice, Freezer, FrozenDevice, Resizer, Uploading, UploadingGuard,
};
use anyhow::{Error, anyhow, ensure};
use fxfs::errors::FxfsError;
use fxfs::filesystem::{FxFilesystem, FxFilesystemBuilder, OpenFxFilesystem, SyncOptions};
use fxfs::object_store::NewChildStoreOptions;
use fxfs::object_store::journal::{DEFAULT_RECLAIM_SIZE, JournalOptions, LazyFlush};
use fxfs::object_store::volume::root_volume;
pub use fxfs_posix::{Attr, DaxExtent, ReaddirStart, SetAttr, StatFs};
use fxfs_posix::{Platform, SyncCounts};
use linux_rt::Task;
use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use storage_device::DeviceHolder;

/// A volume of a filesystem, as the kernel mounts it.
pub type Fxfs = fxfs_posix::Fxfs<Guest>;

/// The volume cladefs's images keep a filesystem's files in (crates/cladefs's `VOLUME_NAME`), which
/// a mount naming none mounts (fs/cladefs/cladefs.h's CLADEFS_DEFAULT_VOLUME).
const VOLUME: &str = "default";

/// When the journal is written, but for syncs: metadata operations stay in memory, as ext4's do
/// between its commits. A write of the journal after data seals the extents open (a barrier): at
/// 128 KiB, that was every few dozen files.
const LAZY_FLUSH: LazyFlush = LazyFlush { bytes: 1 << 20, delay: Duration::from_secs(5) };

/// The share of memory, and of the device, the journal takes, at most `JOURNAL_MAX`: Fxfs flushes
/// its trees, compacting their layers, as each half of the journal fills, and keeps what the
/// journal holds in their in-memory layers meanwhile. Upstream's (`DEFAULT_RECLAIM_SIZE`, 512 KiB)
/// has it compact after each few thousand metadata operations; much more than a few MiB makes
/// the in-memory layers deep, and each change to them seeks further.
const JOURNAL_SHARE: u64 = 32;
const JOURNAL_MAX: u64 = 8 << 20;

/// The share of memory the stores' caches of objects' records take, all told
/// (`object_cache_items`), at about `CACHED_OBJECT_SIZE` bytes each: each use of an inode's
/// attributes not cached looks its record up in the store's tree.
const CACHE_SHARE: u64 = 64;
const CACHED_OBJECT_SIZE: u64 = 256;

/// How long the graveyard gathers the objects the kernel evicts, unlinked, before freeing them:
/// rather than being scheduled for each. Briefly: Fxfs doesn't free them sooner for want of space,
/// and a cell nearly full after a crash may have too little to compact (fxfs-guest-crash did,
/// waiting 100 ms).
const GRAVEYARD_BATCH_DELAY: Duration = Duration::from_millis(5);

/// cladefs: what a shrink leaves free at least, beyond what the filesystem holds: room for its
/// transactions' reservations, which removing a file takes too.
const SHRINK_HEADROOM: u64 = 16 << 20;

/// The filesystem on a device, which its mounted volumes share.
pub struct Filesystem {
    /// The filesystem, which `close` closes, after the last unmount.
    filesystem: OpenFxFilesystem,
    dax: Dax,
    /// What makes the device's commits uploaded, for `sync`.
    uploading: Uploading,
    /// cladefs: what the device's requests took, and what waited on its seals (`stats`).
    device_stats: Arc<DeviceStats>,
    /// cladefs: fsyncs, and those another's sync covered, of all its volumes (`stats`).
    sync_counts: Arc<SyncCounts>,
    /// cladefs: what resizes the device, as the host resizes its cell (`resize`).
    resizer: Resizer,
    /// cladefs: what freezes the device with the filesystem (`freeze`).
    freezer: Freezer,
    /// cladefs: serializes freeze and thaw transitions so no two callers interleave steps.
    freeze_lock: futures::lock::Mutex<()>,
    /// cladefs: single authoritative freeze-thaw state across VFS (`super.c`), DAX (`dax.c`),
    /// Fxfs (`fs.rs`), and the extent device (`device.rs`). Never held across `.await`.
    freeze_state: Mutex<FreezeState>,
}

/// cladefs: the filesystem's single authoritative freeze-thaw state.
///
/// Transition order:
/// - `Active` -> `Frozen(Frozen::DeviceFrozen)` (`Filesystem::freeze`): pauses compactions,
///   flushes graveyard, syncs, holds commits, syncs background frees, flushes uploaded, freezes the
///   device (returning `FrozenDevice` holding its gate closed); read-only skips journal/commits.
/// - `Frozen(Frozen::DeviceFrozen)` -> `Frozen(Frozen::JournalRotating)` (`thaw` step 1):
///   consumes `FrozenDevice` via `unfreeze`, updating the allocator's cell while the device gate
///   is closed and then reopening the gate (read-only finishes directly to `Active`).
/// - `Frozen(Frozen::JournalRotating)` -> `Active` (`thaw` step 2): rotates the journal span onto
///   the new cell, releases held commits, and resumes compactions.
///
/// Until the last step succeeds, the filesystem remains frozen, as the VFS keeps a superblock
/// whose thaw failed, and the next thaw resumes from the stopped step without repeating completed
/// steps (TASK-200).
enum FreezeState {
    Active,
    Frozen(Frozen),
    /// A thaw transition is currently running under `freeze_lock` (still frozen to observers).
    Thawing,
}

enum Frozen {
    /// Step 1 of thaw pending: the device gate is held closed by `device` and the host was told it
    /// is frozen; commits are held by `commits` (if writable) and compactions paused.
    DeviceFrozen { device: FrozenDevice, commits: Option<Task<()>> },
    /// Step 2 of thaw pending: the device is open again on its cell and commits are still held.
    /// Next: rotating the journal's span onto the new cell, then letting commits go on.
    JournalRotating { commits: Task<()> },
}

impl Frozen {
    /// The step it is at, as `cladefs.fail_thaw_at` names it (`FAIL_THAW_AT`).
    fn step(&self) -> u32 {
        match self {
            Frozen::DeviceFrozen { .. } => 1,
            Frozen::JournalRotating { .. } => 2,
        }
    }
}

/// cladefs: fault injection (TASK-200): the next thaw fails before its step this names
/// (`Frozen::step`), which is 0 again then; 0, none.
pub(crate) static FAIL_THAW_AT: AtomicU32 = AtomicU32::new(0);

/// What the guest kernel gives a mounted volume: its filesystem's.
pub struct Guest {
    filesystem: Arc<Filesystem>,
}

impl Platform for Guest {
    const KEEPS_DIRECTORY_TIMES: bool = true;

    type Durable = UploadingGuard;

    fn durable(&self) -> UploadingGuard {
        self.filesystem.uploading.begin()
    }

    fn spawn_urgent(&self, future: impl Future<Output = ()> + Send + 'static) {
        linux_rt::spawn_urgent(future).detach();
    }

    fn sync_counts(&self) -> Arc<SyncCounts> {
        self.filesystem.sync_counts.clone()
    }
}

impl Filesystem {
    /// Opens the filesystem on `device`, sizing what it keeps in memory by the kernel's `memory`
    /// bytes. A device whose cell holds nothing yet is formatted, with an empty default volume,
    /// first. cladefs: read-only, if `read_only`: decided once, here, as the filesystem owns it
    /// (`FxFilesystem::options`), whose volumes and device derive theirs from it (TASK-160.26).
    pub async fn open(device: ExtentDevice, memory: u64, read_only: bool) -> Result<Self, Error> {
        let format = !read_only && device.is_empty().await?;
        let device = if read_only { device.writing_nothing() } else { device };
        let dax = device.dax();
        let uploading = device.uploading();
        let device_stats = device.stats();
        let resizer = device.resizer();
        let freezer = device.freezer();
        let reclaim_size = (memory.min(device.capacity()) / JOURNAL_SHARE)
            .clamp(DEFAULT_RECLAIM_SIZE, JOURNAL_MAX);
        let journal = JournalOptions {
            reclaim_size,
            chunk_size: (reclaim_size / 16).max(131_072),
            barriers_enabled: true,
            lazy_flush: Some(LAZY_FLUSH),
        };
        let filesystem = FxFilesystemBuilder::new()
            .read_only(read_only)
            .format(format)
            .lazy_volumes(true)
            .object_cache_items((memory / CACHE_SHARE / CACHED_OBJECT_SIZE) as usize)
            .graveyard_batch_delay(GRAVEYARD_BATCH_DELAY)
            .journal_options(journal)
            // cladefs: ExtentDevice honors WriteFlags::PRE_BARRIER (`WRITE_BARRIER` + `Seal`), so
            // journaled data writes need no per-4 KiB Fletcher64 checksums or extra recorded
            // flushes (board 6011).
            .barriers_enabled(true)
            .open(DeviceHolder::new(device))
            .await?;
        if format {
            let volume = async {
                let root_volume = root_volume(filesystem.clone()).await?;
                root_volume.new_volume(VOLUME, NewChildStoreOptions::default()).await
            }
            .await;
            if let Err(error) = volume {
                if let Err(close_error) = filesystem.close().await {
                    log::error!("failed to close the filesystem: {close_error:#}");
                }
                return Err(error);
            }
        }
        Ok(Self {
            filesystem,
            dax,
            uploading,
            device_stats,
            sync_counts: Arc::default(),
            resizer,
            freezer,
            freeze_lock: futures::lock::Mutex::new(()),
            freeze_state: Mutex::new(FreezeState::Active),
        })
    }

    /// Closes the filesystem, writing back what is dirty, once no volume of it is mounted.
    pub async fn close(self) -> Result<(), Error> {
        // Release any held freeze guard and commit-hold task so closing cannot deadlock if a final
        // emergency thaw failed before the last volume left, and best-effort tell the host device
        // it thawed before closing (hotbed reviews 91 and 92).
        let state = std::mem::replace(&mut *self.freeze_state.lock().unwrap(), FreezeState::Active);
        match state {
            FreezeState::Frozen(Frozen::DeviceFrozen { device, commits }) => {
                if let Err((device, error)) =
                    device.unfreeze(|cell| self.filesystem.allocator().set_cell(cell)).await
                {
                    log::warn!("cladefs: unfreezing device during close failed: {error:#}");
                    drop(device);
                }
                drop(commits);
            }
            FreezeState::Frozen(Frozen::JournalRotating { commits }) => {
                drop(commits);
            }
            FreezeState::Active | FreezeState::Thawing => {}
        }
        self.filesystem.close().await
    }

    /// Mounts its volume `name`: read-only, if `read_only` or the filesystem is.
    pub async fn mount(self: &Arc<Self>, name: &str, read_only: bool) -> Result<Fxfs, Error> {
        let filesystem = (*self.filesystem).clone();
        let root_volume = root_volume(filesystem.clone()).await?;
        let volume = root_volume.volume(name).await?;
        let guest = Guest { filesystem: self.clone() };
        Ok(Fxfs::new(filesystem, volume, read_only, guest))
    }

    /// Whether it changes nothing: mounted so first. The filesystem's own, which it owns.
    pub fn read_only(&self) -> bool {
        self.filesystem.options().read_only
    }

    /// cladefs: whether the filesystem is frozen (including partially thawed after a failed thaw).
    /// This is the single authoritative freeze state queried by `super.c`, `dax.c`, and ioctls.
    pub fn is_frozen(&self) -> bool {
        !matches!(*self.freeze_state.lock().unwrap(), FreezeState::Active)
    }

    /// cladefs: freezes the filesystem, each of whose volumes the VFS has synced and stopped the
    /// writers of (fsfreeze): holds its journal's compactions, syncs, holds its commits, journals
    /// what committed meanwhile, and closes the device to writes, frees and commits, sealing what
    /// is open: the moment a snapshot of the VM is taken at, which nothing changes until it thaws
    /// (TASK-39.1). Its other background work, freeing and unlinking, waits at the device or to
    /// commit. One that writes nothing, opened read-only, has none of those to stop: it closes and
    /// tells the device alone (TASK-202).
    pub async fn freeze(&self) -> Result<(), Error> {
        let _guard = self.freeze_lock.lock().await;
        ensure!(
            matches!(*self.freeze_state.lock().unwrap(), FreezeState::Active),
            FxfsError::InvalidArgs
        );
        if self.read_only() {
            let device = self.freezer.freeze().await?;
            *self.freeze_state.lock().unwrap() =
                FreezeState::Frozen(Frozen::DeviceFrozen { device, commits: None });
            return Ok(());
        }
        let journal = self.filesystem.journal();
        journal.pause_compactions().await;
        let frozen = async {
            self.filesystem.graveyard().flush().await;
            let options = SyncOptions { flush_device: true, ..Default::default() };
            self.filesystem.sync(options).await?;
            let commits = hold_commits(&self.filesystem).await;
            // As `commits` holds the commit lock: a sync recording reaches would wait for it for
            // ever (TASK-233).
            let options = SyncOptions {
                flush_device: true,
                background_frees: true,
                holding_commits: true,
                ..Default::default()
            };
            self.filesystem.sync(options).await?;
            // Then uploaded, once: the syncs' flushes ordered, each uploaded commit waiting for
            // the bucket (TASK-371).
            {
                let _uploading = self.uploading.begin();
                self.filesystem.device().flush().await?;
            }
            let device = self.freezer.freeze().await?;
            *self.freeze_state.lock().unwrap() =
                FreezeState::Frozen(Frozen::DeviceFrozen { device, commits: Some(commits) });
            anyhow::Ok(())
        }
        .await;
        if frozen.is_err() {
            journal.resume_compactions();
        }
        frozen
    }

    /// cladefs: thaws the filesystem: tells the device, and takes its cell, which is its
    /// own in a VM restored from a snapshot taken frozen: the allocator mints extents of it,
    /// and the journal continues in new ones of it before anything else commits, which a
    /// super-block then records. Then it compacts again. A step that fails leaves it frozen at
    /// that step (`Frozen`), which the next thaw goes on from, never taking one twice: the device
    /// may be unreachable meanwhile, as its daemon restarts (TASK-200).
    pub async fn thaw(&self) -> Result<(), Error> {
        let _guard = self.freeze_lock.lock().await;
        let mut frozen = {
            let mut state = self.freeze_state.lock().unwrap();
            match std::mem::replace(&mut *state, FreezeState::Thawing) {
                FreezeState::Frozen(frozen) => frozen,
                other => {
                    *state = other;
                    return Err(FxfsError::InvalidArgs.into());
                }
            }
        };
        loop {
            frozen = match self.thaw_step(frozen).await {
                Ok(Some(next)) => next,
                Ok(None) => break,
                Err((stopped, error)) => {
                    let step = stopped.step();
                    *self.freeze_state.lock().unwrap() = FreezeState::Frozen(stopped);
                    return Err(error.context(format!("thawing, at step {step}: still frozen")));
                }
            };
        }
        *self.freeze_state.lock().unwrap() = FreezeState::Active;
        self.filesystem.journal().resume_compactions();
        if let Err(error) = self.filesystem.compact_spans_if_needed().await {
            log::warn!("cladefs: post-thaw span compaction failed: {error:#}");
        }
        Ok(())
    }

    /// cladefs: the step of a thaw `frozen` is at, taken: what is frozen after it, if anything; or,
    /// failed, `frozen` as it was, and why.
    async fn thaw_step(&self, frozen: Frozen) -> Result<Option<Frozen>, (Frozen, Error)> {
        if FAIL_THAW_AT
            .compare_exchange(frozen.step(), 0, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            let step = frozen.step();
            return Err((frozen, anyhow!("cladefs.fail_thaw_at: failing before step {step}")));
        }
        match frozen {
            Frozen::DeviceFrozen { device, commits } => {
                match device.unfreeze(|cell| self.filesystem.allocator().set_cell(cell)).await {
                    Ok(()) => {
                        // One that writes nothing has no journal to renew.
                        Ok(commits.map(|commits| Frozen::JournalRotating { commits }))
                    }
                    Err((device, error)) => Err((Frozen::DeviceFrozen { device, commits }, error)),
                }
            }
            Frozen::JournalRotating { commits } => {
                match self.filesystem.rotate_journal_span().await {
                    // Commits go on as the task holding them is dropped, which cancels it.
                    Ok(()) => {
                        drop(commits);
                        Ok(None)
                    }
                    Err(error) => Err((
                        Frozen::JournalRotating { commits },
                        error.context("rotating the journal's span as the filesystem thaws"),
                    )),
                }
            }
        }
    }

    /// cladefs: takes the size the host resized the cell to, as the device's configuration says,
    /// while mounted: grows the device and the allocator; or, asked to shrink, shrinks them if the
    /// allocator holds no more than the smaller size, and answers whether it did.
    pub async fn resize(&self) -> Result<(), Error> {
        // Serialize with freeze/thaw and defer while frozen: thaw schedules and flushes resize once
        // the filesystem is active again (board 12113).
        let _guard = self.freeze_lock.lock().await;
        if self.is_frozen() {
            return Ok(());
        }
        let resizer = &self.resizer;
        let Some((capacity, asked)) = resizer.configured() else { return Ok(()) };
        let allocator = self.filesystem.allocator();
        let current = resizer.capacity();
        if asked != 0 && asked < current {
            // Room left for transactions' reservations, so that files can still be removed.
            let held = allocator.get_used_bytes().0;
            let took = match held.checked_add(SHRINK_HEADROOM).filter(|needed| *needed <= asked) {
                None => {
                    log::info!("not shrinking to {asked} bytes: the filesystem holds {held}");
                    false
                }
                Some(_) => match allocator.set_device_size(asked) {
                    Ok(()) => true,
                    Err(error) => {
                        log::info!("not shrinking to {asked} bytes: {error:#}");
                        false
                    }
                },
            };
            if took {
                resizer.set_capacity(asked);
            }
            return resizer.answer(asked, took).await;
        }
        if capacity > current {
            allocator.set_device_size(capacity)?;
            resizer.set_capacity(capacity);
        }
        Ok(())
    }

    /// The device, as C has it, for DAX (`struct cladefs_device`).
    pub fn device(&self) -> *mut core::ffi::c_void {
        self.dax.device()
    }

    /// cladefs: what the filesystem waited on, and its device's requests took, as lines of a name
    /// and a number: counts, and their nanoseconds, all told since it opened (TASK-91.7).
    pub fn stats(&self) -> String {
        use std::fmt::Write as _;
        use std::sync::atomic::Ordering::Relaxed;
        let device = &self.device_stats;
        let journal = self.filesystem.journal().stats();
        let (fsyncs, coalesced) = self.sync_counts.counts();
        let mut out = String::new();
        let timed = [
            ("device_reads", &device.reads),
            ("device_writes", &device.writes),
            ("device_commits", &device.commits),
            ("seal_waits", &device.seal_waits),
            ("seals", &device.seals),
            ("writes_held_by_seals", &device.writes_held),
        ];
        for (name, timed) in timed {
            let _ = writeln!(out, "{name} {}", timed.count.load(Relaxed));
            let _ = writeln!(out, "{name}_ns {}", timed.ns.load(Relaxed));
            let _ = writeln!(out, "{name}_hist {}", timed.hist());
        }
        let _ = writeln!(out, "fsyncs {fsyncs}");
        // cladefs: the volumes open, mounted or not yet closed (TASK-136.6).
        let _ = writeln!(out, "volumes_open {}", self.filesystem.object_manager().open_volumes());
        let _ = writeln!(out, "fsyncs_coalesced {coalesced}");
        let counts = [
            ("device_read_bytes", &device.read_bytes),
            ("device_write_bytes", &device.write_bytes),
            ("device_uploaded_commits", &device.uploaded_commits),
            ("journal_space_waits", &journal.space_waits),
            ("journal_space_wait_ns", &journal.space_wait_ns),
            ("journal_compactions", &journal.compactions),
            ("journal_compaction_ns", &journal.compaction_ns),
        ];
        for (name, count) in counts {
            let _ = writeln!(out, "{name} {}", count.load(Relaxed));
        }
        // What the filesystem holds back from its free space to write the journal's changes to
        // its trees (`ObjectManager::required_reservation`): it grows with the journal and shrinks
        // as the trees are flushed (TASK-149.25).
        let objects = self.filesystem.object_manager();
        let reserved = objects.metadata_reservation().amount();
        let _ = writeln!(out, "metadata_reserved_bytes {reserved}");
        // And what else the free space a statfs reports changes by, all of it: Fxfs's own
        // metadata, held (the journal, and the trees' layer files, which a flush writes as the
        // reservation shrinks), and the volumes' data, the staging extent among it as far as it
        // has reached; reservations, and allocations not yet committed (TASK-257).
        let usage = self.filesystem.allocator().usage();
        let metadata = [objects.root_parent_store_object_id(), objects.root_store_object_id()];
        let (metadata, data): (Vec<_>, Vec<_>) =
            usage.held.iter().partition(|(owner, _)| metadata.contains(owner));
        let held = |owners: Vec<(&u64, &u64)>| owners.into_iter().map(|(_, bytes)| bytes).sum();
        let lines = [
            ("used_bytes", usage.used()),
            ("metadata_held_bytes", held(metadata)),
            ("data_held_bytes", held(data) + usage.pending),
            ("reserved_bytes", usage.reserved),
            ("uncommitted_bytes", usage.uncommitted),
        ];
        for (name, bytes) in lines {
            let _ = writeln!(out, "{name} {bytes}");
        }
        out
    }
}

/// What a mounted volume does that Fxfs's POSIX layer doesn't.
pub(crate) trait GuestFxfs: Sized {
    /// Unmounts the volume, applying what it queued, and closes it, so that what it holds in
    /// memory goes (TASK-136.6). Its filesystem stays open.
    async fn unmount(self);
}

impl GuestFxfs for Fxfs {
    async fn unmount(self) {
        let (filesystem, volume) = (self.filesystem().clone(), self.volume_id());
        let frozen = self.platform().filesystem.is_frozen();
        self.close().await;
        // One in use, or that a read-only filesystem can't flush, stays open: it holds memory, and
        // nothing else. While the filesystem is frozen, `commits` holds `commit_mutex`, so
        // `close_volume` (which locks commits) would deadlock (hotbed review 91, finding 328).
        if frozen {
            return;
        }
        if let Err(error) = filesystem.object_manager().close_volume(volume).await {
            log::info!("volume {volume} stays open as it unmounts: {error:#}");
        }
    }
}

/// cladefs: holds `filesystem`'s commits until the task returned is dropped.
async fn hold_commits(filesystem: &Arc<FxFilesystem>) -> Task<()> {
    let (held, holding) = futures::channel::oneshot::channel();
    let filesystem = filesystem.clone();
    let task = linux_rt::spawn(async move {
        let _commits = filesystem.lock_commits().await;
        let _ = held.send(());
        futures::future::pending::<()>().await;
    });
    let _ = holding.await;
    task
}
