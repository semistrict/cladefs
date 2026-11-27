// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Fuchsia's `src/storage/fxfs/src/filesystem.rs`, without trimming (the trim task, its options and
// the power manager it waited for a charger with): the device is an extent store (backlog doc-4),
// whose extents Fxfs frees whole once nothing refers to them. cladefs: a commit holds back a
// snapshot of a store it changes, and waits for one (`LockKey::Commits`, TASK-241).

use crate::errors::FxfsError;
use crate::fsck::{FsckOptions, fsck_volume_with_options, fsck_with_options};
use crate::hooks::HooksHandle;
use crate::log::*;
use crate::object_store::ObjectStore;
use crate::object_store::allocator::{Allocator, Hold, Reservation};
use crate::object_store::directory::Directory;
use crate::object_store::graveyard::Graveyard;
use crate::object_store::journal::super_block::SuperBlockHeader;
use crate::object_store::journal::{Journal, JournalCheckpoint, JournalOptions};
use crate::object_store::object_manager::ObjectManager;
use crate::object_store::transaction::{
    self, AssocObj, LockKey, LockManager, MetadataReservation, Mutation,
    TRANSACTION_METADATA_MAX_AMOUNT, Transaction, WriteGuard, lock_keys,
};
use crate::object_store::tree_cache::{TreeCache, TreeCacheBudget};
use crate::object_store::volume::VOLUMES_DIRECTORY;
use crate::serialized_types::Version;
use anyhow::{Context, Error, bail};
use async_trait::async_trait;
use event_listener::Event;
use fuchsia_async as fasync;
use fuchsia_sync::Mutex;
use futures::future::BoxFuture;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use storage_device::{Device, DeviceHolder};
use storage_units::BlockSize;

pub const MIN_BLOCK_SIZE: BlockSize = BlockSize::SIZE_4KIB;
pub const MAX_BLOCK_SIZE: BlockSize = BlockSize::SIZE_64KIB;

// Whilst Fxfs could support up to u64::MAX, off_t is i64 so allowing files larger than that becomes
// difficult to deal with via the POSIX APIs. cladefs: the last page's too, as Linux's MAX_LFS_FILESIZE
// has it, which only Fuchsia's PagedObjectHandle couldn't write: nothing bounded a write by the
// limit, and a file that reached it couldn't be opened once it was closed (xfstests generic/525).
pub const MAX_FILE_SIZE: u64 = i64::MAX as u64;

// The maximum number of transactions that can be in-flight at any time.
const MAX_IN_FLIGHT_TRANSACTIONS: u64 = 4;

/// Holds information on an Fxfs Filesystem
pub struct Info {
    pub total_bytes: u64,
    pub used_bytes: u64,
}

pub type PostCommitHook = Option<Box<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>>;

pub struct Options {
    /// True if the filesystem is read-only.
    pub read_only: bool,

    /// Hooks for filesystem events (e.g. pre_commit, before_commit).
    pub hooks: Arc<HooksHandle>,

    /// A callback that runs after every transaction has been committed.  This will be called whilst
    /// a lock is held which will block more transactions from being committed.
    pub post_commit_hook: PostCommitHook,

    /// If true, don't do an initial reap of the graveyard at mount time.  This is useful for
    /// testing.
    pub skip_initial_reap: bool,

    // Configures the filesystem to use barriers instead of checksums to ensure consistency.
    // Checksums may be computed and stored in extent records but will no longer be stored in the
    // journal. The journal will use barriers to enforce proper ordering between data and metadata
    // writes.
    pub barriers_enabled: bool,

    /// cladefs: how many records of objects (and their extended attributes and keys) the stores
    /// cache, all told, whatever their number: each saves a lookup in its store's tree for each use
    /// of a cached object's attributes.
    pub object_cache_items: usize,

    /// cladefs: if set, the graveyard tombstones the objects queued in batches, waiting this long
    /// once it has none left before taking more, rather than being woken for each: unless
    /// something waits for it (`Graveyard::flush`).
    pub graveyard_batch_delay: Option<std::time::Duration>,

    /// cladefs: if set, mounting opens only the volumes replay needs: the others stay closed, costing
    /// nothing, until they are asked for by name (`RootVolume::volume`). What walks every volume
    /// (fsck, say) doesn't find those. A volume closed holds nothing in the journal.
    pub lazy_volumes: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            read_only: false,
            hooks: Arc::<HooksHandle>::default(),
            post_commit_hook: None,
            skip_initial_reap: false,
            barriers_enabled: false,
            object_cache_items: crate::object_store::tree_cache::ITEM_LIMIT,
            graveyard_batch_delay: None,
            lazy_volumes: false,
        }
    }
}

/// The context in which a transaction is being applied.
pub struct ApplyContext<'a, 'b> {
    /// The mode indicates whether the transaction is being replayed.
    pub mode: ApplyMode<'a, 'b>,

    /// The transaction checkpoint for this mutation.
    pub checkpoint: JournalCheckpoint,
}

/// A transaction can be applied during replay or on a live running system (in which case a
/// transaction object will be available).
pub enum ApplyMode<'a, 'b> {
    Replay,
    Live(&'a Transaction<'b>),
}

impl ApplyMode<'_, '_> {
    pub fn is_replay(&self) -> bool {
        matches!(self, ApplyMode::Replay)
    }

    pub fn is_live(&self) -> bool {
        matches!(self, ApplyMode::Live(_))
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ForceMajor {
    True,
    False,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FlushReason {
    /// Journal memory or space pressure.
    ///
    /// cladefs: `UpgradeVersion` is gone (doc-28, board 10050): frozen layers stay readable in
    /// place through `versioned_type!` and are never rewritten on open to convert them.
    Journal(ForceMajor),
}

/// Objects that use journaling to track mutations (`Allocator` and `ObjectStore`) implement this.
/// This is primarily used by `ObjectManager` and `SuperBlock` with flush calls used in a few tests.
#[async_trait]
pub trait JournalingObject: Send + Sync {
    /// This method get called when the transaction commits, which can either be during live
    /// operation (See `ObjectManager::apply_mutation`) or during journal replay, in which case
    /// transaction will be None (See `super_block::read`).
    fn apply_mutation(
        &self,
        mutation: Mutation,
        context: &ApplyContext<'_, '_>,
        assoc_obj: AssocObj<'_>,
    ) -> Result<(), Error>;

    /// Called when a transaction fails to commit.
    fn drop_mutation(&self, mutation: Mutation, transaction: &Transaction<'_>);

    /// Flushes in-memory changes to the device (to allow journal space to be freed).
    ///
    /// Also returns the earliest version of a struct in the filesystem.
    async fn flush(&self, reason: FlushReason) -> Result<Version, Error>;
}

#[derive(Default)]
pub struct SyncOptions<'a> {
    /// If set, the journal will be flushed, as well as the underlying block device.  This is much
    /// more expensive, but ensures the contents of the journal are persisted (which also acts as a
    /// barrier, ensuring all previous journal writes are observable by future operations).
    /// Note that when this is not set, the journal is *not* synchronously flushed by the sync call,
    /// and it will return before the journal flush completes.  In other words, some journal
    /// mutations may still be buffered in memory after this call returns.
    pub flush_device: bool,

    /// A precondition that is evaluated whilst a lock is held that determines whether or not the
    /// sync needs to proceed.
    pub precondition: Option<Box<dyn FnOnce() -> bool + 'a + Send>>,

    /// cladefs: with `flush_device`, return without waiting for the frees of the extents that the
    /// flush lets die, round trips to the device that the next flush waits for (as an fsync
    /// needs only what it wrote to be on the device).
    pub background_frees: bool,

    /// cladefs: the caller holds the commit lock: an allocation for the journal's own transaction
    /// (`Transaction::holding_commits`), short of room, as a commit makes room in the journal. The
    /// sync records no reaches, whose transaction would wait for that lock, its own task's, for
    /// ever (TASK-233); the next sync records them.
    pub holding_commits: bool,
}

pub struct OpenFxFilesystem(Arc<FxFilesystem>);

impl OpenFxFilesystem {
    /// Waits for filesystem to be dropped (so callers should ensure all direct and indirect
    /// references are dropped) and returns the device.  No attempt is made at a graceful shutdown.
    pub async fn take_device(self) -> DeviceHolder {
        let fut = self.device.take_when_dropped();
        std::mem::drop(self);
        debug_assert_not_too_long!(fut)
    }
}

impl From<Arc<FxFilesystem>> for OpenFxFilesystem {
    fn from(fs: Arc<FxFilesystem>) -> Self {
        Self(fs)
    }
}

impl Drop for OpenFxFilesystem {
    fn drop(&mut self) {
        if !self.options.read_only && !self.closed.load(Ordering::SeqCst) {
            error!("OpenFxFilesystem dropped without first being closed. Data loss may occur.");
        }
    }
}

impl std::ops::Deref for OpenFxFilesystem {
    type Target = Arc<FxFilesystem>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub struct FxFilesystemBuilder {
    format: bool,
    options: Options,
    journal_options: JournalOptions,
    on_new_allocator: Option<Box<dyn Fn(Arc<Allocator>) + Send + Sync>>,
    on_new_store: Option<Box<dyn Fn(&ObjectStore) + Send + Sync>>,
    fsck_after_every_transaction: bool,
}

impl FxFilesystemBuilder {
    pub fn new() -> Self {
        Self {
            format: false,
            options: Options::default(),
            journal_options: JournalOptions::default(),
            on_new_allocator: None,
            on_new_store: None,
            fsck_after_every_transaction: false,
        }
    }

    /// Sets whether the block device should be formatted when opened. Defaults to `false`.
    pub fn format(mut self, format: bool) -> Self {
        self.format = format;
        self
    }

    /// Sets whether the filesystem will be opened in read-only mode. Defaults to `false`.
    /// Incompatible with `format`.
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.options.read_only = read_only;
        self
    }

    /// cladefs: sets how many objects' records the stores cache (`Options::object_cache_items`).
    pub fn object_cache_items(mut self, items: usize) -> Self {
        self.options.object_cache_items = items;
        self
    }

    /// cladefs: has the graveyard tombstone in batches (`Options::graveyard_batch_delay`).
    pub fn graveyard_batch_delay(mut self, delay: std::time::Duration) -> Self {
        self.options.graveyard_batch_delay = Some(delay);
        self
    }

    /// cladefs: opens volumes as they are asked for (`Options::lazy_volumes`).
    pub fn lazy_volumes(mut self, lazy: bool) -> Self {
        self.options.lazy_volumes = lazy;
        self
    }

    /// Sets a callback that runs after every transaction has been committed. See
    /// `Options::post_commit_hook`.
    pub fn post_commit_hook(
        mut self,
        hook: impl Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync + 'static,
    ) -> Self {
        self.options.post_commit_hook = Some(Box::new(hook));
        self
    }

    /// Sets whether to do an initial reap of the graveyard at mount time. See
    /// `Options::skip_initial_reap`. Defaults to `false`.
    pub fn skip_initial_reap(mut self, skip_initial_reap: bool) -> Self {
        self.options.skip_initial_reap = skip_initial_reap;
        self
    }

    /// Sets the options for the journal.
    pub fn journal_options(mut self, journal_options: JournalOptions) -> Self {
        self.journal_options = journal_options;
        self
    }

    /// Sets a method to be called immediately after creating the allocator.
    pub fn on_new_allocator(
        mut self,
        on_new_allocator: impl Fn(Arc<Allocator>) + Send + Sync + 'static,
    ) -> Self {
        self.on_new_allocator = Some(Box::new(on_new_allocator));
        self
    }

    /// Sets a method to be called each time a new store is registered with `ObjectManager`.
    pub fn on_new_store(
        mut self,
        on_new_store: impl Fn(&ObjectStore) + Send + Sync + 'static,
    ) -> Self {
        self.on_new_store = Some(Box::new(on_new_store));
        self
    }

    /// Enables or disables running fsck after every transaction. Defaults to `false`.
    pub fn fsck_after_every_transaction(mut self, fsck_after_every_transaction: bool) -> Self {
        self.fsck_after_every_transaction = fsck_after_every_transaction;
        self
    }

    /// Enables or disables barriers in both the filesystem and journal options.
    /// Defaults to `false`.
    pub fn barriers_enabled(mut self, barriers_enabled: bool) -> Self {
        self.options.barriers_enabled = barriers_enabled;
        self.journal_options.barriers_enabled = barriers_enabled;
        self
    }

    pub fn hooks(mut self, hooks: Arc<crate::hooks::HooksHandle>) -> Self {
        self.options.hooks = hooks;
        self
    }

    /// Constructs an `FxFilesystem` object with the specified settings.
    pub async fn open(self, device: DeviceHolder) -> Result<OpenFxFilesystem, Error> {
        let read_only = self.options.read_only;
        if self.format && read_only {
            bail!("Cannot initialize a filesystem as read-only");
        }

        let device_block_size =
            BlockSize::new(device.block_size()).expect("Device block size is not a power of 2");
        let block_size = std::cmp::max(device_block_size, MIN_BLOCK_SIZE);
        assert!(block_size <= MAX_BLOCK_SIZE, "Max supported block size is 64KiB");

        let objects = Arc::new(ObjectManager::new(block_size, self.on_new_store));
        let journal = Arc::new(Journal::new(
            objects.clone(),
            self.journal_options,
            self.options.hooks.clone(),
        ));

        let mut fsck_after_every_transaction = None;
        let mut filesystem_options = self.options;
        if self.fsck_after_every_transaction {
            let instance =
                FsckAfterEveryTransaction::new(filesystem_options.post_commit_hook.take());
            fsck_after_every_transaction = Some(instance.clone());
            filesystem_options.post_commit_hook =
                Some(Box::new(move || Box::pin(instance.clone().run())));
        }

        if !read_only && !self.format {
            // See comment in JournalRecord::DidFlushDevice for why we need to flush the device
            // before replay.
            device.flush().await.context("Device flush failed")?;
        }

        let filesystem = Arc::new(FxFilesystem {
            device,
            block_size,
            objects: objects.clone(),
            journal,
            commit_mutex: futures::lock::Mutex::new(()),
            fsck_reach_lock: futures::lock::Mutex::new(()),
            lock_manager: LockManager::new(),
            flush_task: Mutex::new(None),
            closed: AtomicBool::new(true),
            tree_cache_budget: TreeCacheBudget::new(filesystem_options.object_cache_items),
            graveyard: Graveyard::new(objects.clone(), filesystem_options.graveyard_batch_delay),
            options: filesystem_options,
            in_flight_transactions: AtomicU64::new(0),
            transaction_limit_event: Event::new(),
        });

        if self.format {
            let journal = filesystem.journal.clone();
            let fs = filesystem.clone();
            crate::offload(move || async move { journal.init_empty(fs).await }).await?;
            let journal = filesystem.journal.clone();
            crate::offload(move || async move { journal.init_superblock().await }).await?;

            // Start the graveyard's background reaping task.
            filesystem.graveyard.clone().reap_async();

            // Create the root volume directory.
            let root_store = filesystem.root_store();
            let root_directory = crate::boxed(|| {
                Directory::open(&root_store, root_store.root_directory_object_id())
            })
            .await
            .context("Unable to open root volume directory")?;
            let mut transaction = crate::boxed(|| {
                root_store.new_transaction(
                    lock_keys![LockKey::object(
                        root_store.store_object_id(),
                        root_directory.object_id()
                    )],
                    transaction::Options::default(),
                )
            })
            .await?;
            let volume_directory = crate::boxed(|| {
                root_directory.create_child_dir(&mut transaction, VOLUMES_DIRECTORY)
            })
            .await?;
            crate::boxed(|| transaction.commit()).await?;
            objects.set_volume_directory(volume_directory);
        } else {
            let journal = filesystem.journal.clone();
            let fs = filesystem.clone();
            let on_new_allocator = self.on_new_allocator;
            crate::offload(move || async move { journal.replay(fs, on_new_allocator).await })
                .await
                .context("Journal replay failed")?;

            let compacts = !read_only;
            if compacts {
                filesystem.journal.pause_compactions().await;
            }
            let mounted = async {
                if compacts {
                    if filesystem.allocator().has_dead_extents() {
                        filesystem.maybe_start_flush_task();
                        let journal = &filesystem.journal;
                        crate::boxed(|| journal.flush_for_frees())
                            .await
                            .context("freeing what the crash left")?;
                    }
                    filesystem.compact_spans_if_needed().await?;
                }

                if !read_only {
                    for store in objects.stores() {
                        let graveyard = filesystem.graveyard.clone();
                        crate::offload(move || async move { graveyard.initial_reap(&store).await })
                            .await?;
                    }
                }
                anyhow::Ok(())
            }
            .await;
            if compacts {
                filesystem.journal.resume_compactions();
            }
            mounted.context("Failed to go on from the journal")?;
        }

        // This must be after we've formatted the filesystem; it will fail during format otherwise.
        if let Some(fsck_after_every_transaction) = fsck_after_every_transaction {
            fsck_after_every_transaction
                .fs
                .set(Arc::downgrade(&filesystem))
                .unwrap_or_else(|_| unreachable!());
        }

        filesystem.closed.store(false, Ordering::SeqCst);

        if !read_only {
            // Start the background tasks.
            filesystem.graveyard.clone().reap_async();
        }

        Ok(filesystem.into())
    }
}

pub struct FxFilesystem {
    block_size: BlockSize,
    objects: Arc<ObjectManager>,
    journal: Arc<Journal>,
    commit_mutex: futures::lock::Mutex<()>,
    /// cladefs: serializes locked `fsck` checks against `Journal::flush` reach commits
    /// (`record_reaches_for_flush`) without ever requiring `flush_task` to wait on `commit_mutex`
    /// (strict lock DAG: `commit_mutex` -> `fsck_reach_lock` -> `writer_mutex`, preventing FIFO
    /// and `no_lock` `fsck` AB-BA deadlocks with `holding_commits` syncs; board 7617, board 11651).
    fsck_reach_lock: futures::lock::Mutex<()>,
    lock_manager: LockManager,
    flush_task: Mutex<Option<fasync::Task<()>>>,
    closed: AtomicBool,
    graveyard: Arc<Graveyard>,
    /// cladefs: what the caches of its stores' records share (`Options::object_cache_items`).
    tree_cache_budget: Arc<TreeCacheBudget>,
    options: Options,

    // The number of in-flight transactions which we will limit to MAX_IN_FLIGHT_TRANSACTIONS.
    in_flight_transactions: AtomicU64,

    // An event that is used to wake up tasks that are blocked due to the in-flight transaction
    // limit.
    transaction_limit_event: Event,

    // NOTE: This *must* go last so that when users take the device from a closed filesystem, the
    // filesystem has dropped all other members first (Rust drops members in declaration order).
    device: DeviceHolder,
}

impl FxFilesystem {
    pub async fn new_empty(device: DeviceHolder) -> Result<OpenFxFilesystem, Error> {
        FxFilesystemBuilder::new().format(true).open(device).await
    }

    pub async fn open(device: DeviceHolder) -> Result<OpenFxFilesystem, Error> {
        FxFilesystemBuilder::new().open(device).await
    }

    pub fn root_parent_store(&self) -> Arc<ObjectStore> {
        self.objects.root_parent_store()
    }

    pub async fn close(&self) -> Result<(), Error> {
        assert_eq!(self.closed.swap(true, Ordering::SeqCst), false);
        debug_assert_not_too_long!(self.graveyard.wait_for_reap());
        // cladefs: what dropped allocations left in shared extents, which the next mount counts.
        // Do not return early on failure: close must still flush the journal, terminate the flush
        // task, and close the device (TASK-623, board 2644).
        let reach_status =
            if self.options().read_only { Ok(()) } else { self.allocator().record_reaches().await };
        if let Err(e) = &reach_status {
            error!(error:? = e; "Failed to record reaches during close");
        }
        self.journal.stop_compactions().await;
        let sync_status = if self.options().read_only {
            Ok(None)
        } else {
            match self.journal.sync(SyncOptions { flush_device: true, ..Default::default() }).await
            {
                // cladefs: the frees the sync let die, before the device closes.
                Ok(checkpoint) => self.allocator().freed().await.map(|()| checkpoint),
                Err(error) => Err(error),
            }
        };
        // cladefs: with nothing in flight, what the filesystem holds, its metadata reservation
        // included, which the next mount reserves again, fits in the device: one holding more
        // counted bytes it didn't hold, and its next mount fails (TASK-211).
        if let Ok(Some(_)) = &sync_status {
            let allocator = self.allocator();
            let (used, disk) = (allocator.get_used_bytes().0, allocator.get_disk_bytes());
            debug_assert!(
                used <= disk,
                "the filesystem holds {used} bytes of a device of {disk}: {}",
                allocator.owner_bytes_debug()
            );
        }
        match &sync_status {
            Ok(None) => {}
            Ok(checkpoint) => info!(
                "Filesystem closed (checkpoint={}, metadata_reservation={:?}, \
                 reservation_required={}, borrowed={})",
                checkpoint.as_ref().unwrap().0.file_offset,
                self.object_manager().metadata_reservation(),
                self.object_manager().required_reservation(),
                self.object_manager().borrowed_metadata_space(),
            ),
            Err(e) => error!(error:? = e; "Failed to sync filesystem; data may be lost"),
        }
        self.journal.terminate();
        let flush_task = self.flush_task.lock().take();
        if let Some(task) = flush_task {
            debug_assert_not_too_long!(task);
        }
        // Regardless of whether sync succeeds, we should close the device, since otherwise we will
        // crash instead of exiting gracefully.
        self.device().close().await.context("Failed to close device")?;
        reach_status.and(sync_status.map(|_| ()))
    }

    pub fn device(&self) -> Arc<dyn Device> {
        Arc::clone(&self.device)
    }

    pub fn root_store(&self) -> Arc<ObjectStore> {
        self.objects.root_store()
    }

    pub fn allocator(&self) -> Arc<Allocator> {
        self.objects.allocator()
    }

    pub fn object_manager(&self) -> &Arc<ObjectManager> {
        &self.objects
    }

    pub fn journal(&self) -> &Arc<Journal> {
        &self.journal
    }

    pub async fn sync(&self, options: SyncOptions<'_>) -> Result<(), Error> {
        let wait_for_frees = options.flush_device && !options.background_frees;
        // cladefs: what dropped allocations left in shared extents, before the barrier that seals it:
        // boxed, as an allocation short of room syncs, and this commits a transaction; when the
        // caller already holds the commit lock (`options.holding_commits`, TASK-233), commit
        // directly under `writer_mutex` rather than skipping reaches (board 11651).
        if !self.options.read_only && !options.holding_commits {
            Box::pin(self.allocator().record_reaches()).await?;
        } else if !self.options.read_only {
            Box::pin(self.allocator().record_reaches_holding_commits()).await?;
        }
        self.journal.sync(options).await?;
        if wait_for_frees {
            self.allocator().freed().await.context("Failed to free dead extents")?;
        }
        Ok(())
    }

    pub fn block_size(&self) -> BlockSize {
        self.block_size
    }

    pub fn get_info(&self) -> Info {
        Info {
            total_bytes: self.device.size(),
            used_bytes: self.object_manager().allocator().get_used_bytes().0,
        }
    }

    pub fn super_block_header(&self) -> SuperBlockHeader {
        self.journal.super_block_header()
    }

    pub fn graveyard(&self) -> &Arc<Graveyard> {
        &self.graveyard
    }

    pub fn options(&self) -> &Options {
        &self.options
    }

    /// cladefs: a new cache of a store's records.
    pub fn new_tree_cache(&self) -> TreeCache {
        TreeCache::sharing(&self.tree_cache_budget)
    }

    /// Returns a guard that must be taken before any transaction can commence.  This guard takes a
    /// shared lock on the filesystem.  `fsck` will take an exclusive lock so that it can get a
    /// consistent picture of the filesystem that it can verify.  It is important that this lock is
    /// acquired before *all* other locks.  It is also important that this lock is not taken twice
    /// by the same task since that can lead to deadlocks if another task tries to take a write
    /// lock.
    pub async fn lock_commits(&self) -> futures::lock::MutexGuard<'_, ()> {
        self.commit_mutex.lock().await
    }

    /// cladefs: whether `close()` has begun (so `Journal::flush` does not retry `record_reaches`
    /// after `close()` already attempted it; TASK-623, board 11651).
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// cladefs: serializes `record_reaches_for_flush` against locked `fsck` without touching
    /// `commit_mutex` (board 7617, board 11651).
    pub(crate) async fn lock_fsck_for_reaches(&self) -> futures::lock::MutexGuard<'_, ()> {
        self.fsck_reach_lock.lock().await
    }

    /// cladefs: acquires `commit_mutex` and then `fsck_reach_lock` for a locked `fsck` (board
    /// 7617, board 11651).
    pub(crate) async fn lock_commits_for_fsck(&self) -> FsckCommitGuard<'_> {
        let _commits = self.commit_mutex.lock().await;
        let _reaches = self.fsck_reach_lock.lock().await;
        FsckCommitGuard { _reaches, _commits }
    }

    /// cladefs: compacts if accumulated `Root::journal_spans` reached `COMPACT_SPAN_THRESHOLD`,
    /// starting the flush task if needed (mount / thaw).
    pub async fn compact_spans_if_needed(&self) -> Result<(), Error> {
        if self.journal.needs_span_compaction() {
            self.maybe_start_flush_task();
            crate::boxed(|| self.journal.force_compact())
                .await
                .context("compacting accumulated journal spans")?;
        }
        Ok(())
    }

    /// cladefs: rotates the journal to a fresh position-derived span on the current device cell
    /// when thawing in a VM restored from a snapshot onto a forked cell (board 6923). No-op if the
    /// cell has not changed. Nothing may commit or compact meanwhile.
    pub async fn rotate_journal_span(&self) -> Result<(), Error> {
        crate::boxed(|| self.journal.rotate_journal_span()).await
    }

    /// cladefs: all extents known to the filesystem (allocator extents + live/unfreed system
    /// extents: active/retired super-block and unfreed journal blocks), for leak oracles
    /// (boards 6923, 6926).
    pub fn known_extents(&self) -> rustc_hash::FxHashMap<storage_device::ExtentId, u64> {
        let mut known: rustc_hash::FxHashMap<storage_device::ExtentId, u64> =
            self.allocator().extents().into_iter().map(|(e, (_, len))| (e, len)).collect();
        for (extent, len) in self.journal.known_extents() {
            known.entry(extent).and_modify(|v| *v = (*v).max(len)).or_insert(len);
        }
        known
    }

    /// cladefs: bytes counted for `extent` across both the allocator and system extents
    /// (super-block and journal blocks), for `counts_what_its_device_holds` (boards 6923, 6926).
    pub fn extent_counted(&self, extent: storage_device::ExtentId) -> u64 {
        let alloc = self.allocator().extent_counted(extent);
        let sys = self.journal.known_extents().get(&extent).copied().unwrap_or(0);
        alloc.max(sys)
    }

    pub async fn commit_transaction<R: Send>(
        &self,
        transaction: &mut Transaction<'_>,
        callback: impl FnOnce(u64) -> R + Send,
    ) -> Result<R, Error> {
        self.hooks().on_pre_commit(transaction)?;
        debug_assert_not_too_long!(self.lock_manager.commit_prepare(&transaction));

        // cladefs: a store's commits, which a snapshot of it holds back from its flush until it
        // commits (TASK-241), held until the transaction is committed. A flush's BeginFlush and
        // EndFlush take nothing, as the snapshot's flush makes them. Since transaction.mutations()
        // is sorted by object_id, we can deduplicate on-the-fly.
        let mut commits = Vec::new();
        let mut last_object_id = 0;
        for mutation in transaction.mutations() {
            let object_id = mutation.object_id;
            if matches!(mutation.mutation, Mutation::BeginFlush | Mutation::EndFlush) {
                continue;
            }

            if object_id == last_object_id {
                continue;
            }
            assert!(object_id > last_object_id);
            last_object_id = object_id;
            if self.object_manager().store(object_id).is_some() {
                let commits_of = lock_keys![LockKey::commits(object_id)];
                commits.push(self.lock_manager.read_lock(commits_of).await);
            }
        }

        self.maybe_start_flush_task();

        self.hooks().on_before_commit();

        let _guard = debug_assert_not_too_long!(self.commit_mutex.lock());
        let journal_offset = self.journal.commit(transaction).await?;

        std::mem::drop(commits);

        // For now, call the callback whilst holding the lock.  Technically, we don't need to do
        // that except if there's a post-commit-hook (which there usually won't be).  We can
        // consider changing this if we need to for performance, but we'd need to double check that
        // callers don't depend on this.
        let result = callback(journal_offset);

        if let Some(hook) = self.options.post_commit_hook.as_ref() {
            hook().await;
        }

        Ok(result)
    }

    pub fn lock_manager(&self) -> &LockManager {
        &self.lock_manager
    }

    pub fn hooks(&self) -> &Arc<HooksHandle> {
        &self.options.hooks
    }

    pub(crate) fn drop_transaction(&self, transaction: &mut Transaction<'_>) {
        if !matches!(transaction.metadata_reservation, MetadataReservation::None) {
            self.sub_transaction();
        }
        // If we placed a hold for metadata space, return it now.
        if let MetadataReservation::Hold(hold_amount) =
            std::mem::replace(&mut transaction.metadata_reservation, MetadataReservation::None)
        {
            let hold = transaction
                .allocator_reservation
                .unwrap()
                .reserve(0)
                .expect("Zero should always succeed.");
            hold.add(hold_amount);
        }
        self.objects.drop_transaction(transaction);
        self.lock_manager.drop_transaction(transaction);
    }

    fn maybe_start_flush_task(&self) {
        let mut flush_task = self.flush_task.lock();
        if flush_task.is_none() {
            let journal = self.journal.clone();
            *flush_task = Some(fasync::Task::spawn(journal.flush_task()));
        }
    }

    /// cladefs: waits for room in the journal, which only the flush task's compactions make: it is
    /// started first, as a mount that has committed nothing yet has none, and a wait without it
    /// never ends (TASK-272).
    pub(crate) async fn wait_for_journal_space(&self) -> Result<(), Error> {
        self.maybe_start_flush_task();
        self.journal.check_journal_space().await
    }

    pub(crate) async fn reservation_for_transaction<'a>(
        self: &Arc<Self>,
        options: transaction::Options<'a>,
    ) -> Result<(MetadataReservation, Option<&'a Reservation>, Option<Hold<'a>>), Error> {
        if !options.skip_journal_checks {
            self.wait_for_journal_space().await?;
        }

        // We support three options for metadata space reservation:
        //
        //   1. We can borrow from the filesystem's metadata reservation.  This should only be
        //      be used on the understanding that eventually, potentially after a full compaction,
        //      there should be no net increase in space used.  For example, unlinking an object
        //      should eventually decrease the amount of space used and setting most attributes
        //      should not result in any change.
        //
        //   2. A reservation is provided in which case we'll place a hold on some of it for
        //      metadata.
        //
        //   3. No reservation is supplied, so we try and reserve space with the allocator now,
        //      and will return NoSpace if that fails.
        //
        // cladefs: a transaction that may borrow reserves with the allocator if it can, unless it
        // compacts or brings a reservation of its own, and borrows only what that doesn't pay
        // for (`MetadataReservation::Borrowing`): all of it if the filesystem is too full. A
        // borrowing transaction is charged its journal usage four times over, the padding of the
        // syncs before it included, which only compacting the journal repays: the graveyard's
        // tombstones, and unlinks, between fsyncs, drained the metadata reservation of the
        // guest's large journal faster than it compacted (PostgreSQL's initdb failed with ENOSPC,
        // then the compaction itself).
        let mut hold = None;
        let reserved = if options.borrow_metadata_space
            && !options.skip_journal_checks
            && options.allocator_reservation.is_none()
        {
            self.allocator().reserve(None, TRANSACTION_METADATA_MAX_AMOUNT)
        } else {
            None
        };
        let metadata_reservation = if let Some(reservation) = reserved {
            MetadataReservation::Borrowing(reservation)
        } else if options.borrow_metadata_space {
            MetadataReservation::Borrowed
        } else {
            match options.allocator_reservation {
                Some(reservation) => {
                    hold = Some(
                        reservation
                            .reserve(TRANSACTION_METADATA_MAX_AMOUNT)
                            .ok_or(FxfsError::NoSpace)?,
                    );
                    MetadataReservation::Hold(TRANSACTION_METADATA_MAX_AMOUNT)
                }
                None => {
                    let reservation = self
                        .allocator()
                        .reserve(None, TRANSACTION_METADATA_MAX_AMOUNT)
                        .ok_or(FxfsError::NoSpace)?;
                    MetadataReservation::Reservation(reservation)
                }
            }
        };
        Ok((metadata_reservation, options.allocator_reservation, hold))
    }

    /// cladefs: renews the metadata reservation of `transaction`, committed and continued
    /// (`Transaction::commit_and_continue`), to what a new transaction's holds
    /// (TRANSACTION_METADATA_MAX_AMOUNT): its commit moved what it used of it to the metadata
    /// reservation, and what it commits next must be paid for again. Without this, a truncate of
    /// a file of many extents, which trims them in a transaction continued many times, ran the
    /// reservation dry and panicked ("Insufficient reservation space", TASK-58). NoSpace if the
    /// allocator, or the transaction's own reservation, has no room for it.
    pub(crate) fn renew_metadata_reservation(
        &self,
        transaction: &mut Transaction<'_>,
    ) -> Result<(), Error> {
        match &mut transaction.metadata_reservation {
            MetadataReservation::None | MetadataReservation::Borrowed => {}
            MetadataReservation::Reservation(reservation) => {
                let used = TRANSACTION_METADATA_MAX_AMOUNT.saturating_sub(reservation.amount());
                if used > 0 {
                    let renewed = self.allocator().reserve(None, used).ok_or(FxfsError::NoSpace)?;
                    renewed.move_to(reservation, used);
                }
            }
            // It borrows what its reservation doesn't pay for: renewed if the allocator has room.
            MetadataReservation::Borrowing(reservation) => {
                let used = TRANSACTION_METADATA_MAX_AMOUNT.saturating_sub(reservation.amount());
                if let Some(renewed) = self.allocator().reserve(None, used).filter(|_| used > 0) {
                    renewed.move_to(reservation, used);
                }
            }
            MetadataReservation::Hold(hold_amount) => {
                let used = TRANSACTION_METADATA_MAX_AMOUNT.saturating_sub(*hold_amount);
                if used > 0 {
                    let reservation = transaction.allocator_reservation.unwrap();
                    reservation.reserve(used).ok_or(FxfsError::NoSpace)?.forget();
                    *hold_amount = TRANSACTION_METADATA_MAX_AMOUNT;
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn add_transaction(&self, skip_journal_checks: bool) {
        if skip_journal_checks {
            self.in_flight_transactions.fetch_add(1, Ordering::Relaxed);
        } else {
            let inc = || {
                let mut in_flights = self.in_flight_transactions.load(Ordering::Relaxed);
                while in_flights < MAX_IN_FLIGHT_TRANSACTIONS {
                    match self.in_flight_transactions.compare_exchange_weak(
                        in_flights,
                        in_flights + 1,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => return true,
                        Err(x) => in_flights = x,
                    }
                }
                return false;
            };
            while !inc() {
                let listener = self.transaction_limit_event.listen();
                if inc() {
                    break;
                }
                listener.await;
            }
        }
    }

    pub(crate) fn sub_transaction(&self) {
        let old = self.in_flight_transactions.fetch_sub(1, Ordering::Relaxed);
        assert!(old != 0);
        if old <= MAX_IN_FLIGHT_TRANSACTIONS {
            self.transaction_limit_event.notify(usize::MAX);
        }
    }

    pub async fn truncate_guard(&self, store_id: u64, object_id: u64) -> TruncateGuard<'_> {
        let keys = lock_keys![LockKey::truncate(store_id, object_id,)];
        TruncateGuard(self.lock_manager().write_lock(keys).await)
    }

    pub async fn truncate_guard_owned(
        self: &Arc<Self>,
        store_id: u64,
        object_id: u64,
    ) -> TruncateGuard<'static> {
        self.truncate_guard(store_id, object_id).await.into_owned(self.clone())
    }
}

/// A wrapper around a guard that needs to be taken when truncating an object.
#[allow(dead_code)]
pub struct TruncateGuard<'a>(WriteGuard<'a>);

/// cladefs: holds `fsck_reach_lock` and `commit_mutex` for the duration of a locked `fsck`
/// (dropped in reverse order: `_reaches` before `_commits`; board 7617, board 11651).
pub(crate) struct FsckCommitGuard<'a> {
    _reaches: futures::lock::MutexGuard<'a, ()>,
    _commits: futures::lock::MutexGuard<'a, ()>,
}

impl<'a> TruncateGuard<'a> {
    pub fn into_owned(self, fs: Arc<FxFilesystem>) -> TruncateGuard<'static> {
        TruncateGuard(self.0.into_owned(fs))
    }
}

/// Helper method for making a new filesystem.
pub async fn mkfs(device: DeviceHolder) -> Result<DeviceHolder, Error> {
    let fs = FxFilesystem::new_empty(device).await?;
    fs.close().await?;
    Ok(fs.take_device().await)
}

struct FsckAfterEveryTransaction {
    fs: OnceLock<Weak<FxFilesystem>>,
    old_hook: PostCommitHook,
}

impl FsckAfterEveryTransaction {
    fn new(old_hook: PostCommitHook) -> Arc<Self> {
        Arc::new(Self { fs: OnceLock::new(), old_hook })
    }

    async fn run(self: Arc<Self>) {
        if let Some(fs) = self.fs.get().and_then(Weak::upgrade) {
            // cladefs: says what it found, not "1 error".
            let issues = Arc::new(Mutex::new(Vec::new()));
            let found = issues.clone();
            let options = FsckOptions {
                fail_on_warning: true,
                no_lock: true,
                quiet: true,
                on_error: Box::new(move |issue| found.lock().push(format!("{issue:?}"))),
                ..Default::default()
            };
            if let Err(error) = fsck_with_options(fs.clone(), &options).await {
                panic!("fsck after a transaction: {error:#}: {:?}", issues.lock());
            }
            let object_manager = fs.object_manager();
            for store in object_manager.stores() {
                let store_id = store.store_object_id();
                if !object_manager.is_system_store(store_id) {
                    let checked = fsck_volume_with_options(fs.as_ref(), &options, store_id);
                    if let Err(error) = checked.await {
                        panic!(
                            "fsck of {store_id} after a transaction: {error:#}: {:?}",
                            issues.lock()
                        );
                    }
                }
            }
        }
        if let Some(old_hook) = self.old_hook.as_ref() {
            old_hook().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FxFilesystem, FxFilesystemBuilder, FxfsError, SyncOptions};
    use crate::fsck::{fsck, fsck_volume};
    use crate::log::*;
    use crate::lsm_tree::Operation;
    use crate::lsm_tree::types::Item;
    use crate::object_handle::{INVALID_OBJECT_ID, ObjectHandle, WriteObjectHandle};
    use crate::object_store::directory::{Directory, replace_child};
    use crate::object_store::journal::JournalOptions;
    use crate::object_store::transaction::{LockKey, Options, lock_keys};
    use crate::object_store::volume::root_volume;
    use crate::object_store::{HandleOptions, NewChildStoreOptions, ObjectStore};
    use fuchsia_async as fasync;
    use fuchsia_sync::Mutex;
    use futures::future::join_all;
    use futures::stream::{FuturesUnordered, TryStreamExt};
    use rustc_hash::FxHashMap as HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;
    use storage_device::DeviceHolder;
    use storage_device::ExtentId;
    use storage_device::fake_device::{self, FakeDevice};
    use test_case::test_case;

    const TEST_DEVICE_BLOCK_SIZE: u32 = 512;

    #[fuchsia_async::run(10, test)]
    async fn test_compaction() {
        let device = DeviceHolder::new(FakeDevice::new(8192, TEST_DEVICE_BLOCK_SIZE));

        // If compaction is not working correctly, this test will run out of space.
        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");
        let root_store = fs.root_store();
        let root_directory = Directory::open(&root_store, root_store.root_directory_object_id())
            .await
            .expect("open failed");

        let mut tasks = Vec::new();
        for i in 0..2 {
            let mut transaction = fs
                .root_store()
                .new_transaction(
                    lock_keys![LockKey::object(
                        root_store.store_object_id(),
                        root_directory.object_id()
                    )],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            let handle = root_directory
                .create_child_file(&mut transaction, &format!("{}", i))
                .await
                .expect("create_child_file failed");
            transaction.commit().await.expect("commit failed");
            tasks.push(fasync::Task::spawn(async move {
                const TEST_DATA: &[u8] = b"hello";
                let mut buf = handle.allocate_buffer(TEST_DATA.len()).await;
                buf.copy_from_slice(TEST_DATA);
                for _ in 0..1500 {
                    handle.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");
                }
            }));
        }
        join_all(tasks).await;
        fs.sync(SyncOptions::default()).await.expect("sync failed");

        fsck(fs.clone()).await.expect("fsck failed");
        fs.close().await.expect("Close failed");
    }

    #[fuchsia_async::run(10, test)]
    async fn test_replay_is_identical() {
        let device = DeviceHolder::new(FakeDevice::new(8192, TEST_DEVICE_BLOCK_SIZE));
        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");

        // Reopen the store, but set reclaim size to a very large value which will effectively
        // stop the journal from flushing and allows us to track all the mutations to the store.
        fs.close().await.expect("close failed");
        let device = fs.take_device().await;
        device.reopen(false);

        struct Mutations<K, V>(Mutex<Vec<(Operation, Item<K, V>)>>);

        impl<K: Clone, V: Clone> Mutations<K, V> {
            fn new() -> Self {
                Mutations(Mutex::new(Vec::new()))
            }

            fn push(&self, operation: Operation, item: &Item<K, V>) {
                self.0.lock().push((operation, item.clone()));
            }
        }

        let open_fs = |device,
                       object_mutations: Arc<Mutex<HashMap<_, _>>>,
                       allocator_mutations: Arc<Mutations<_, _>>| async {
            FxFilesystemBuilder::new()
                .journal_options(JournalOptions { reclaim_size: u64::MAX, ..Default::default() })
                .on_new_allocator(move |allocator| {
                    let allocator_mutations = allocator_mutations.clone();
                    allocator.tree().set_mutation_callback(Some(Box::new(move |op, item| {
                        allocator_mutations.push(op, item)
                    })));
                })
                .on_new_store(move |store| {
                    let mutations = Arc::new(Mutations::new());
                    object_mutations.lock().insert(store.store_object_id(), mutations.clone());
                    store.tree().set_mutation_callback(Some(Box::new(move |op, item| {
                        mutations.push(op, item)
                    })));
                })
                .open(device)
                .await
                .expect("open failed")
        };

        let allocator_mutations = Arc::new(Mutations::new());
        let object_mutations = Arc::new(Mutex::new(HashMap::default()));
        let fs = open_fs(device, object_mutations.clone(), allocator_mutations.clone()).await;

        let root_store = fs.root_store();
        let root_directory = Directory::open(&root_store, root_store.root_directory_object_id())
            .await
            .expect("open failed");

        let mut transaction = fs
            .root_store()
            .new_transaction(
                lock_keys![LockKey::object(
                    root_store.store_object_id(),
                    root_directory.object_id()
                )],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        let object = root_directory
            .create_child_file(&mut transaction, "test")
            .await
            .expect("create_child_file failed");
        transaction.commit().await.expect("commit failed");

        // Append some data.
        let buf = object.allocate_buffer(10000).await;
        object.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");

        // Overwrite some data.
        object.write_or_append(Some(5000), buf.as_ref()).await.expect("write failed");

        // Truncate.
        object.truncate(3000).await.expect("truncate failed");

        // Delete the object.
        let mut transaction = fs
            .root_store()
            .new_transaction(
                lock_keys![
                    LockKey::object(root_store.store_object_id(), root_directory.object_id()),
                    LockKey::object(root_store.store_object_id(), object.object_id()),
                ],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");

        replace_child(&mut transaction, None, (&root_directory, "test"))
            .await
            .expect("replace_child failed");

        transaction.commit().await.expect("commit failed");

        // Finally tombstone the object.
        root_store
            .tombstone_object(object.object_id(), Options::default(), None)
            .await
            .expect("tombstone failed");

        // Now reopen and check that replay produces the same set of mutations.
        fs.close().await.expect("close failed");

        let metadata_reservation_amount = fs.object_manager().metadata_reservation().amount();

        let device = fs.take_device().await;
        device.reopen(false);

        let replayed_object_mutations = Arc::new(Mutex::new(HashMap::default()));
        let replayed_allocator_mutations = Arc::new(Mutations::new());
        let fs = open_fs(
            device,
            replayed_object_mutations.clone(),
            replayed_allocator_mutations.clone(),
        )
        .await;

        let m1 = object_mutations.lock();
        let m2 = replayed_object_mutations.lock();
        assert_eq!(m1.len(), m2.len());
        for (store_id, mutations) in &*m1 {
            let mutations = mutations.0.lock();
            let replayed = m2.get(&store_id).expect("Found unexpected store").0.lock();
            assert_eq!(mutations.len(), replayed.len());
            for ((op1, i1), (op2, i2)) in mutations.iter().zip(replayed.iter()) {
                assert_eq!(op1, op2);
                assert_eq!(i1.key, i2.key);
                assert_eq!(i1.value, i2.value);
            }
        }

        let a1 = allocator_mutations.0.lock();
        let a2 = replayed_allocator_mutations.0.lock();
        assert_eq!(a1.len(), a2.len());
        for ((op1, i1), (op2, i2)) in a1.iter().zip(a2.iter()) {
            assert_eq!(op1, op2);
            assert_eq!(i1.key, i2.key);
            assert_eq!(i1.value, i2.value);
        }

        assert_eq!(
            fs.object_manager().metadata_reservation().amount(),
            metadata_reservation_amount
        );
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_max_in_flight_transactions() {
        let device = DeviceHolder::new(FakeDevice::new(8192, TEST_DEVICE_BLOCK_SIZE));
        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");

        let store = fs.root_store();
        let transactions = FuturesUnordered::new();
        for _ in 0..super::MAX_IN_FLIGHT_TRANSACTIONS {
            transactions.push(store.new_transaction(lock_keys![], Options::default()));
        }
        let mut transactions: Vec<_> = transactions.try_collect().await.unwrap();

        // Trying to create another one should be blocked.
        let mut fut = std::pin::pin!(store.new_transaction(lock_keys![], Options::default()));
        assert!(futures::poll!(&mut fut).is_pending());

        // Dropping one should allow it to proceed.
        transactions.pop();

        assert!(futures::poll!(&mut fut).is_ready());
    }

    #[test_case(true; "test power fail with barriers")]
    #[test_case(false; "test power fail with checksums")]
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_power_fail(barriers_enabled: bool) {
        // This test randomly discards blocks, so we run it a few times to increase the chances
        // of catching an issue in a single run.
        for _ in 0..10 {
            let (store_id, device, test_file_object_id) = {
                let device = DeviceHolder::new(FakeDevice::new(8192, 4096));
                let fs = if barriers_enabled {
                    FxFilesystemBuilder::new()
                        .barriers_enabled(true)
                        .format(true)
                        .open(device)
                        .await
                        .expect("new filesystem failed")
                } else {
                    FxFilesystem::new_empty(device).await.expect("new_empty failed")
                };
                let root_volume = root_volume(fs.clone()).await.expect("root_volume failed");

                fs.sync(SyncOptions { flush_device: true, ..SyncOptions::default() })
                    .await
                    .expect("sync failed");

                let store = root_volume
                    .new_volume("test", NewChildStoreOptions::default())
                    .await
                    .expect("new_volume failed");
                let root_directory = Directory::open(&store, store.root_directory_object_id())
                    .await
                    .expect("open failed");

                // Create a number of files with the goal of using up more than one journal block.
                async fn create_files(store: &Arc<ObjectStore>, prefix: &str) {
                    let fs = store.filesystem();
                    let root_directory = Directory::open(store, store.root_directory_object_id())
                        .await
                        .expect("open failed");
                    for i in 0..100 {
                        let mut transaction = fs
                            .root_store()
                            .new_transaction(
                                lock_keys![LockKey::object(
                                    store.store_object_id(),
                                    store.root_directory_object_id()
                                )],
                                Options::default(),
                            )
                            .await
                            .expect("new_transaction failed");
                        root_directory
                            .create_child_file(&mut transaction, &format!("{prefix} {i}"))
                            .await
                            .expect("create_child_file failed");
                        transaction.commit().await.expect("commit failed");
                    }
                }

                // Create one batch of files.
                create_files(&store, "A").await;

                // Create a file and write something to it.  This will make sure there's a
                // transaction present that includes a checksum.
                let mut transaction = fs
                    .root_store()
                    .new_transaction(
                        lock_keys![LockKey::object(
                            store.store_object_id(),
                            store.root_directory_object_id()
                        )],
                        Options::default(),
                    )
                    .await
                    .expect("new_transaction failed");
                let object = root_directory
                    .create_child_file(&mut transaction, "test")
                    .await
                    .expect("create_child_file failed");
                transaction.commit().await.expect("commit failed");

                let mut transaction =
                    object.new_transaction().await.expect("new_transaction failed");
                let mut buffer = object.allocate_buffer(4096).await;
                buffer.fill(0xed);
                object
                    .txn_write(&mut transaction, 0, buffer.as_ref())
                    .await
                    .expect("txn_write failed");
                transaction.commit().await.expect("commit failed");

                // Create another batch of files.
                create_files(&store, "B").await;

                // Sync the device, but don't flush the device. We want to do this so we can
                // randomly discard blocks below.
                fs.sync(SyncOptions::default()).await.expect("sync failed");

                // When we call `sync` above on the filesystem, it will pad the journal so that it
                // will get written, but it doesn't wait for the write to occur.  We wait for a
                // short time here to give allow time for the journal to be written.  Adding timers
                // isn't great, but this test already isn't deterministic since we randomly discard
                // blocks.
                fasync::Timer::new(Duration::from_millis(10)).await;

                (
                    store.store_object_id(),
                    fs.device().snapshot().expect("snapshot failed"),
                    object.object_id(),
                )
            };

            // Randomly discard blocks since the last flush.  This simulates what might happen in
            // the case of power-loss.  This will be an uncontrolled unmount.
            device
                .discard_random_since_last_flush()
                .expect("discard_random_since_last_flush failed");

            let fs = FxFilesystem::open(device).await.expect("open failed");
            fsck(fs.clone()).await.expect("fsck failed");

            let mut check_test_file = false;

            // If we replayed and the store exists (i.e. the transaction that created the store
            // made it out), start by running fsck on it.
            let object_id = if fs.object_manager().store(store_id).is_some() {
                fsck_volume(&fs, store_id).await.expect("fsck_volume failed");

                // Now we want to create another file, unmount cleanly, and then finally check that
                // the new file exists.  This checks that we can continue to use the filesystem
                // after an unclean unmount.
                let store = root_volume(fs.clone())
                    .await
                    .expect("root_volume failed")
                    .volume("test")
                    .await
                    .expect("volume failed");

                let root_directory = Directory::open(&store, store.root_directory_object_id())
                    .await
                    .expect("open failed");

                let mut transaction = fs
                    .root_store()
                    .new_transaction(
                        lock_keys![LockKey::object(
                            store.store_object_id(),
                            store.root_directory_object_id()
                        )],
                        Options::default(),
                    )
                    .await
                    .expect("new_transaction failed");
                let object = root_directory
                    .create_child_file(&mut transaction, &format!("C"))
                    .await
                    .expect("create_child_file failed");
                transaction.commit().await.expect("commit failed");

                // Write again to the test file if it exists.
                if let Ok(test_file) =
                    ObjectStore::open_object(&store, test_file_object_id, HandleOptions::default())
                        .await
                {
                    // Check it has the contents we expect.
                    let mut buffer = test_file.allocate_buffer(4096).await;
                    let bytes =
                        test_file.read_aligned(0, buffer.as_mut()).await.expect("read failed");
                    if bytes == 4096 {
                        let expected = [0xed; 4096];
                        assert_eq!(buffer.to_vec(), expected);
                    } else {
                        // If the write didn't make it, the file should have zero bytes.
                        assert_eq!(bytes, 0);
                    }

                    // Modify the test file.
                    let mut transaction =
                        test_file.new_transaction().await.expect("new_transaction failed");
                    buffer.fill(0x37);
                    test_file
                        .txn_write(&mut transaction, 0, buffer.as_ref())
                        .await
                        .expect("txn_write failed");
                    transaction.commit().await.expect("commit failed");
                    check_test_file = true;
                }

                object.object_id()
            } else {
                INVALID_OBJECT_ID
            };

            // This will do a controlled unmount.
            fs.close().await.expect("close failed");
            let device = fs.take_device().await;
            device.reopen(false);

            let fs = FxFilesystem::open(device).await.expect("open failed");
            fsck(fs.clone()).await.expect("fsck failed");

            // As mentioned above, make sure that the object we created before the clean unmount
            // exists.
            if object_id != INVALID_OBJECT_ID {
                fsck_volume(&fs, store_id).await.expect("fsck_volume failed");

                let store = root_volume(fs.clone())
                    .await
                    .expect("root_volume failed")
                    .volume("test")
                    .await
                    .expect("volume failed");
                // We should be able to open the C object.
                ObjectStore::open_object(&store, object_id, HandleOptions::default())
                    .await
                    .expect("open_object failed");

                // If we made the modification to the test file, check it.
                if check_test_file {
                    info!("Checking test file for modification");
                    let test_file = ObjectStore::open_object(
                        &store,
                        test_file_object_id,
                        HandleOptions::default(),
                    )
                    .await
                    .expect("open_object failed");
                    let mut buffer = test_file.allocate_buffer(4096).await;
                    assert_eq!(
                        test_file.read_aligned(0, buffer.as_mut()).await.expect("read failed"),
                        4096
                    );
                    let expected = [0x37; 4096];
                    let data = buffer.to_vec();
                    assert_eq!(data, expected);
                }
            }

            fs.close().await.expect("close failed");
        }
    }

    /// cladefs: the extents of the device's cell that it holds and Fxfs doesn't know.
    async fn leaked_extents(fs: &FxFilesystem) -> Vec<ExtentId> {
        let device = fs.device();
        let known = fs.known_extents();
        let cell = device.cell();
        let held = device.extents().into_iter();
        held.filter(|extent| extent.cell == cell && !known.contains_key(extent)).collect()
    }

    /// cladefs: Fxfs writes, overwrites, renames and deletes files while its store crashes at
    /// every barrier (backlog doc-4: a crash keeps what preceded the last barrier, and any of what
    /// followed it, at random). Each crash leaves a filesystem that mounts and checks, holds the files
    /// synced before it, and goes on writing.
    #[test_case(true, false; "with barriers")]
    #[test_case(true, true; "with barriers on atomic commits")]
    #[test_case(false, false; "with checksums")]
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_crashes_at_random_barriers(barriers_enabled: bool, atomic_commits: bool) {
        const ROUNDS: u64 = 24;
        const VOLUME: &str = "test";
        let crashes = fake_device::Crashes::new(26_3, 1);
        let mut fake = FakeDevice::new(8192, 4096).record_crashes(crashes.clone());
        if atomic_commits {
            fake = fake.with_atomic_commits();
        }
        let pre_barriers = fake.pre_barrier_counter();
        let device = DeviceHolder::new(fake);
        let fs = FxFilesystemBuilder::new()
            .barriers_enabled(barriers_enabled)
            .format(true)
            .open(device)
            .await
            .expect("format failed");
        let store = root_volume(fs.clone())
            .await
            .expect("root_volume failed")
            .new_volume(VOLUME, NewChildStoreOptions::default())
            .await
            .expect("new_volume failed");
        let directory =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");
        let block_size = fs.block_size().get();
        // What file `i` of `round` holds: `i + 1` blocks of a byte of its own, of which a file
        // that is overwritten has its second block replaced (see below).
        let contents = |round: u64, i: u64| {
            let byte = (round * 4 + i) as u8;
            let mut data = vec![byte; ((i + 1) * block_size) as usize];
            if i == 2 {
                data[block_size as usize..2 * block_size as usize].fill(!byte);
            }
            data
        };
        fs.sync(SyncOptions { flush_device: true, ..Default::default() })
            .await
            .expect("sync failed");
        // Crashes from now on hold the volume, and the files of the rounds before the mark.
        crashes.mark(0);

        for round in 0..ROUNDS {
            for i in 0..4 {
                let mut transaction = store
                    .new_transaction(
                        lock_keys![LockKey::object(store.store_object_id(), directory.object_id())],
                        Options::default(),
                    )
                    .await
                    .expect("new_transaction failed");
                let file = directory
                    .create_child_file(&mut transaction, &format!("r{round}f{i}"))
                    .await
                    .expect("create_child_file failed");
                transaction.commit().await.expect("commit failed");
                let data = contents(round, i);
                let byte = data[0];
                let mut buffer = file.allocate_buffer(data.len()).await;
                buffer.copy_from_slice(&data);
                file.write_or_append(Some(0), buffer.as_ref()).await.expect("write failed");
                if i == 2 {
                    let mut buffer = file.allocate_buffer(block_size as usize).await;
                    buffer.fill(!byte);
                    file.write_or_append(Some(block_size), buffer.as_ref())
                        .await
                        .expect("rewrite failed");
                }
            }
            if round > 0 {
                // Renames one of the round before's files, and deletes another.
                let renamed = format!("r{}f0", round - 1);
                let (renamed_id, _) =
                    directory.lookup(&renamed).await.expect("lookup failed").expect("missing");
                let deleted = format!("r{}f1", round - 1);
                let (deleted_id, _) =
                    directory.lookup(&deleted).await.expect("lookup failed").expect("missing");
                let mut transaction = store
                    .new_transaction(
                        lock_keys![
                            LockKey::object(store.store_object_id(), directory.object_id()),
                            LockKey::object(store.store_object_id(), renamed_id),
                            LockKey::object(store.store_object_id(), deleted_id),
                        ],
                        Options::default(),
                    )
                    .await
                    .expect("new_transaction failed");
                replace_child(
                    &mut transaction,
                    Some((&directory, &renamed)),
                    (&directory, &format!("moved{round}")),
                )
                .await
                .expect("rename failed");
                replace_child(&mut transaction, None, (&directory, &deleted))
                    .await
                    .expect("unlink failed");
                transaction.commit().await.expect("commit failed");
                fs.graveyard().queue_tombstone_object(store.store_object_id(), deleted_id);
                fs.graveyard().flush().await;
            }
            if round % 6 == 5 {
                // Super-blocks, and layers compacted into new extents.
                fs.journal().force_compact().await.expect("compact failed");
            }
            fs.sync(SyncOptions { flush_device: true, ..Default::default() })
                .await
                .expect("sync failed");
            crashes.mark(round + 1);
        }
        std::mem::drop(directory);
        std::mem::drop(store);
        fs.close().await.expect("close failed");

        let recorded: Vec<_> = crashes.take().collect();
        assert!(recorded.len() >= ROUNDS as usize, "{} crashes recorded", recorded.len());
        for (crash, (synced_rounds, device)) in recorded.into_iter().enumerate() {
            let fs = FxFilesystemBuilder::new()
                .barriers_enabled(barriers_enabled)
                .open(DeviceHolder::new(device))
                .await
                .unwrap_or_else(|e| panic!("crash {crash} doesn't mount: {e:?}"));
            fsck(fs.clone()).await.unwrap_or_else(|e| panic!("crash {crash}: fsck failed: {e:?}"));
            let store = root_volume(fs.clone())
                .await
                .expect("root_volume failed")
                .volume(VOLUME)
                .await
                .unwrap_or_else(|e| panic!("crash {crash}: volume failed: {e:?}"));
            fsck_volume(&fs, store.store_object_id())
                .await
                .unwrap_or_else(|e| panic!("crash {crash}: fsck_volume failed: {e:?}"));
            let directory = Directory::open(&store, store.root_directory_object_id())
                .await
                .expect("open failed");
            for round in 0..synced_rounds {
                for i in 2..4 {
                    let name = format!("r{round}f{i}");
                    let (id, _) = directory
                        .lookup(&name)
                        .await
                        .expect("lookup failed")
                        .unwrap_or_else(|| panic!("crash {crash}: {name} is missing"));
                    let file = ObjectStore::open_object(&store, id, HandleOptions::default())
                        .await
                        .expect("open_object failed");
                    let data = contents(round, i);
                    let mut buffer = file.allocate_buffer(data.len()).await;
                    file.read_aligned(0, buffer.as_mut()).await.expect("read failed");
                    assert!(buffer.to_vec() == data, "crash {crash}: {name} doesn't hold its data");
                }
            }

            // The filesystem goes on: it writes, compacts, and checks after a remount.
            let mut transaction = store
                .new_transaction(
                    lock_keys![LockKey::object(store.store_object_id(), directory.object_id())],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            let file = directory
                .create_child_file(&mut transaction, "after")
                .await
                .expect("create_child_file failed");
            transaction.commit().await.expect("commit failed");
            let mut buffer = file.allocate_buffer(4 * block_size as usize).await;
            buffer.fill(0xaa);
            file.write_or_append(Some(0), buffer.as_ref())
                .await
                .unwrap_or_else(|e| panic!("crash {crash}: write failed: {e:?}"));
            fs.journal()
                .force_compact()
                .await
                .unwrap_or_else(|e| panic!("crash {crash}: compact failed: {e:?}"));
            std::mem::drop(buffer);
            std::mem::drop(file);
            std::mem::drop(directory);
            std::mem::drop(store);
            fs.close().await.expect("close failed");
            let device = fs.take_device().await;
            device.reopen(false);
            let fs = FxFilesystem::open(device)
                .await
                .unwrap_or_else(|e| panic!("crash {crash} doesn't remount: {e:?}"));
            fsck(fs.clone())
                .await
                .unwrap_or_else(|e| panic!("crash {crash}: fsck after remount failed: {e:?}"));
            // What the crash left that nothing refers to (orphans) is freed.
            fs.sync(SyncOptions { flush_device: true, ..Default::default() })
                .await
                .expect("sync failed");
            assert_eq!(
                leaked_extents(&fs).await,
                vec![],
                "crash {crash}: the store holds extents Fxfs doesn't know"
            );
            fs.close().await.expect("close failed");
        }

        // Checked after every crash recovery and post-crash write/compaction so recovered devices
        // are verified to preserve `atomic_commits()` too (board 7537, hotbed review 10).
        let pre_barriers = pre_barriers.load(std::sync::atomic::Ordering::Relaxed);
        if barriers_enabled && !atomic_commits {
            assert!(pre_barriers > 0, "expected PRE_BARRIER writes on a non-atomic device");
        } else {
            assert_eq!(
                pre_barriers, 0,
                "unexpected PRE_BARRIER writes (barriers_enabled={barriers_enabled}, atomic_commits={atomic_commits})"
            );
        }
    }

    /// cladefs: Fxfs counts space against the store's capacity: it runs out with NoSpace before the
    /// store refuses a write, and the extents of files deleted are freed, which makes room again.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_space_runs_out_at_the_stores_capacity() {
        const MIB: usize = 1 << 20;
        // 16 MiB.
        let device = DeviceHolder::new(FakeDevice::new(4096, 4096));
        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");
        let store = fs.root_store();
        let directory =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");

        // Writes files of a MiB, in writes of 256 KiB, until one fails, and returns the error and
        // the names of the files, of which all but the last were written whole.
        let fill = async |prefix: &str| {
            let mut names = Vec::new();
            loop {
                let name = format!("{prefix}{}", names.len());
                let mut transaction = store
                    .new_transaction(
                        lock_keys![LockKey::object(store.store_object_id(), directory.object_id())],
                        Options::default(),
                    )
                    .await
                    .expect("new_transaction failed");
                let file = directory
                    .create_child_file(&mut transaction, &name)
                    .await
                    .expect("create_child_file failed");
                transaction.commit().await.expect("commit failed");
                let mut buffer = file.allocate_buffer(MIB / 4).await;
                buffer.fill(0x5a);
                names.push(name);
                for i in 0..4 {
                    if let Err(error) =
                        file.write_or_append(Some((i * MIB / 4) as u64), buffer.as_ref()).await
                    {
                        return (error, names);
                    }
                }
            }
        };

        let extents = fs.allocator().extents().len();
        let (error, names) = fill("a").await;
        assert!(FxfsError::NoSpace.matches(&error), "{error:?}");
        // Metadata takes some of the store, and files take the rest.
        let mib = names.len() - 1;
        assert!(mib >= 8, "{mib} MiB written");

        for name in &names {
            let (id, _) = directory.lookup(name).await.expect("lookup failed").expect("missing");
            let mut transaction = store
                .new_transaction(
                    lock_keys![
                        LockKey::object(store.store_object_id(), directory.object_id()),
                        LockKey::object(store.store_object_id(), id),
                    ],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            replace_child(&mut transaction, None, (&directory, name)).await.expect("unlink failed");
            transaction.commit().await.expect("commit failed");
            fs.graveyard().queue_tombstone_object(store.store_object_id(), id);
        }
        fs.graveyard().flush().await;
        fs.sync(SyncOptions { flush_device: true, ..Default::default() })
            .await
            .expect("sync failed");

        // The files' extents are freed, and the room they took is back, but for what metadata grew
        // by meanwhile.
        assert_eq!(fs.allocator().dead_extents(), vec![]);
        assert_eq!(fs.allocator().extents().len(), extents);
        let (error, names) = fill("b").await;
        assert!(FxfsError::NoSpace.matches(&error), "{error:?}");
        assert!(names.len() - 1 >= mib - 1, "{mib} MiB, then {}", names.len() - 1);
        std::mem::drop(directory);
        fsck(fs.clone()).await.expect("fsck failed");
        fs.close().await.expect("close failed");
    }

    /// cladefs: a fork of the store (backlog doc-4) has a cell of its own, and holds the
    /// filesystem as its parent did, which it mounts, changes and checks apart from its parent.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_fork() {
        async fn create(fs: &Arc<FxFilesystem>, name: &str, byte: u8) {
            let store = fs.root_store();
            let directory = Directory::open(&store, store.root_directory_object_id())
                .await
                .expect("open failed");
            let mut transaction = store
                .new_transaction(
                    lock_keys![LockKey::object(store.store_object_id(), directory.object_id())],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            let file = directory
                .create_child_file(&mut transaction, name)
                .await
                .expect("create_child_file failed");
            transaction.commit().await.expect("commit failed");
            let mut buffer = file.allocate_buffer(3 * 4096).await;
            buffer.fill(byte);
            file.write_or_append(Some(0), buffer.as_ref()).await.expect("write failed");
        }

        /// The first byte of `name`, or `None` if there is no such file.
        async fn first_byte(fs: &Arc<FxFilesystem>, name: &str) -> Option<u8> {
            let store = fs.root_store();
            let directory = Directory::open(&store, store.root_directory_object_id())
                .await
                .expect("open failed");
            let (id, _) = directory.lookup(name).await.expect("lookup failed")?;
            let file = ObjectStore::open_object(&store, id, HandleOptions::default())
                .await
                .expect("open_object failed");
            let mut buffer = file.allocate_buffer(4096).await;
            file.read_aligned(0, buffer.as_mut()).await.expect("read failed");
            Some(buffer.to_vec()[0])
        }

        let fs = FxFilesystem::new_empty(DeviceHolder::new(FakeDevice::new(8192, 4096)))
            .await
            .expect("new_empty failed");
        create(&fs, "parent's", 1).await;
        fs.close().await.expect("close failed");
        let device = fs.take_device().await;
        device.reopen(false);
        let fork = device.fork(device.cell() + 1).expect("fork failed");

        // The fork writes its own extents: the journal's tail, which its parent's cell
        // preallocated, and new files.
        let fs = FxFilesystem::open(fork).await.expect("open failed");
        fsck(fs.clone()).await.expect("fsck failed");
        assert_eq!(first_byte(&fs, "parent's").await, Some(1));
        create(&fs, "fork's", 2).await;
        fs.journal().force_compact().await.expect("compact failed");
        fs.close().await.expect("close failed");
        let fork = fs.take_device().await;
        fork.reopen(false);
        let fs = FxFilesystem::open(fork).await.expect("open failed");
        fsck(fs.clone()).await.expect("fsck failed");
        assert_eq!(first_byte(&fs, "fork's").await, Some(2));
        fs.close().await.expect("close failed");

        let fs = FxFilesystem::open(device).await.expect("open failed");
        fsck(fs.clone()).await.expect("fsck failed");
        assert_eq!(first_byte(&fs, "parent's").await, Some(1));
        assert_eq!(first_byte(&fs, "fork's").await, None);
        fs.close().await.expect("close failed");
    }

    /// cladefs: random 4 KiB writes over a large file, each to new blocks that leave the old ones'
    /// extents partly dead, as a guest's mmapped writes make, and the journal compacted.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_compaction_after_random_overwrites() {
        const FILE_SIZE: u64 = 32 << 20;
        const WRITES: u64 = 4096;
        let fs = FxFilesystem::new_empty(DeviceHolder::new(FakeDevice::new(65536, 4096)))
            .await
            .expect("new_empty failed");
        let store = fs.root_store();
        let directory =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");
        let mut transaction = store
            .new_transaction(
                lock_keys![LockKey::object(store.store_object_id(), directory.object_id())],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        let file = directory
            .create_child_file(&mut transaction, "file")
            .await
            .expect("create_child_file failed");
        transaction.commit().await.expect("commit failed");
        let mut buffer = file.allocate_buffer(1 << 20).await;
        buffer.as_mut().fill(1);
        for offset in (0..FILE_SIZE).step_by(1 << 20) {
            file.write_or_append(Some(offset), buffer.as_ref()).await.expect("write failed");
        }
        let mut block = file.allocate_buffer(4096).await;
        block.as_mut().fill(2);
        // A fixed pseudo-random sequence of blocks.
        let mut seed: u64 = 0x2545f4914f6cdd1d;
        for _ in 0..WRITES {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let offset = (seed % (FILE_SIZE / 4096)) * 4096;
            file.write_or_append(Some(offset), block.as_ref()).await.expect("write failed");
        }
        fs.journal().force_compact().await.expect("compact failed");
        fs.close().await.expect("close failed");
        let device = fs.take_device().await;
        device.reopen(false);
        let fs = FxFilesystem::open(device).await.expect("open failed");
        fsck(fs.clone()).await.expect("fsck failed");
        fs.close().await.expect("close failed");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_barrier_not_emitted_when_transaction_has_no_data() {
        let barrier_count = Arc::new(AtomicU32::new(0));

        struct Observer(Arc<AtomicU32>);

        impl fake_device::Observer for Observer {
            fn barrier(&self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let mut fake_device = FakeDevice::new(8192, 4096);
        fake_device.set_observer(Box::new(Observer(barrier_count.clone())));
        let device = DeviceHolder::new(fake_device);
        let fs = FxFilesystemBuilder::new()
            .barriers_enabled(true)
            .format(true)
            .open(device)
            .await
            .expect("new filesystem failed");

        {
            let root_vol = root_volume(fs.clone()).await.expect("root_volume failed");
            root_vol
                .new_volume("test", NewChildStoreOptions { ..NewChildStoreOptions::default() })
                .await
                .expect("there is no test volume");
            fs.close().await.expect("close failed");
        }
        // Remount the filesystem to ensure that the journal flushes and we can get a reliable
        // measure of the number of barriers issued during setup.
        let device = fs.take_device().await;
        device.reopen(false);
        let fs = FxFilesystemBuilder::new()
            .barriers_enabled(true)
            .open(device)
            .await
            .expect("new filesystem failed");
        let expected_barrier_count = barrier_count.load(Ordering::Relaxed);

        let root_vol = root_volume(fs.clone()).await.expect("root_volume failed");
        let store = root_vol.volume("test").await.expect("there is no test volume");

        // Create a number of files with the goal of using up more than one journal block.
        let fs = store.filesystem();
        let root_directory =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");
        for i in 0..100 {
            let mut transaction = fs
                .root_store()
                .new_transaction(
                    lock_keys![LockKey::object(
                        store.store_object_id(),
                        store.root_directory_object_id()
                    )],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            root_directory
                .create_child_file(&mut transaction, &format!("A {i}"))
                .await
                .expect("create_child_file failed");
            transaction.commit().await.expect("commit failed");
        }

        // Unmount the filesystem to ensure that the journal flushes.
        fs.close().await.expect("close failed");
        // Ensure that no barriers were emitted while creating files, as no data was written.
        assert_eq!(expected_barrier_count, barrier_count.load(Ordering::Relaxed));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_barrier_emitted_when_transaction_includes_data() {
        let barrier_count = Arc::new(AtomicU32::new(0));

        struct Observer(Arc<AtomicU32>);

        impl fake_device::Observer for Observer {
            fn barrier(&self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let mut fake_device = FakeDevice::new(8192, 4096);
        fake_device.set_observer(Box::new(Observer(barrier_count.clone())));
        let device = DeviceHolder::new(fake_device);
        let fs = FxFilesystemBuilder::new()
            .barriers_enabled(true)
            .format(true)
            .open(device)
            .await
            .expect("new filesystem failed");

        {
            let root_vol = root_volume(fs.clone()).await.expect("root_volume failed");
            root_vol
                .new_volume("test", NewChildStoreOptions { ..NewChildStoreOptions::default() })
                .await
                .expect("there is no test volume");
            fs.close().await.expect("close failed");
        }
        // Remount the filesystem to ensure that the journal flushes and we can get a reliable
        // measure of the number of barriers issued during setup.
        let device = fs.take_device().await;
        device.reopen(false);
        let fs = FxFilesystemBuilder::new()
            .barriers_enabled(true)
            .open(device)
            .await
            .expect("new filesystem failed");
        let expected_barrier_count = barrier_count.load(Ordering::Relaxed);

        let root_vol = root_volume(fs.clone()).await.expect("root_volume failed");
        let store = root_vol.volume("test").await.expect("there is no test volume");

        // Create a file and write something to it. This should cause a barrier to be emitted.
        let fs: Arc<FxFilesystem> = store.filesystem();
        let root_directory =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");

        let mut transaction = fs
            .root_store()
            .new_transaction(
                lock_keys![LockKey::object(
                    store.store_object_id(),
                    store.root_directory_object_id()
                )],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        let object = root_directory
            .create_child_file(&mut transaction, "test")
            .await
            .expect("create_child_file failed");
        transaction.commit().await.expect("commit failed");

        let mut transaction = object.new_transaction().await.expect("new_transaction failed");
        let mut buffer = object.allocate_buffer(4096).await;
        buffer.fill(0xed);
        object.txn_write(&mut transaction, 0, buffer.as_ref()).await.expect("txn_write failed");
        transaction.commit().await.expect("commit failed");

        // Unmount the filesystem to ensure that the journal flushes.
        fs.close().await.expect("close failed");
        // Ensure that a barrier was emitted while writing to the file.
        assert!(expected_barrier_count < barrier_count.load(Ordering::Relaxed));
    }

    #[test_case(true; "fail when original filesystem has barriers enabled")]
    #[test_case(false; "fail when original filesystem has barriers disabled")]
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_switching_barrier_mode_on_existing_filesystem(original_barrier_mode: bool) {
        let fake_device = FakeDevice::new(8192, 4096);
        let device = DeviceHolder::new(fake_device);
        let fs: super::OpenFxFilesystem = FxFilesystemBuilder::new()
            .barriers_enabled(original_barrier_mode)
            .format(true)
            .open(device)
            .await
            .expect("new filesystem failed");

        // Create a volume named test with a file inside it called file.
        {
            let root_vol = root_volume(fs.clone()).await.expect("root_volume failed");
            let store = root_vol
                .new_volume("test", NewChildStoreOptions::default())
                .await
                .expect("creating test volume");
            let root_dir = Directory::open(&store, store.root_directory_object_id())
                .await
                .expect("open failed");
            let mut transaction = fs
                .root_store()
                .new_transaction(
                    lock_keys![LockKey::object(
                        store.store_object_id(),
                        store.root_directory_object_id()
                    )],
                    Default::default(),
                )
                .await
                .expect("new_transaction failed");
            let object = root_dir
                .create_child_file(&mut transaction, "file")
                .await
                .expect("create_child_file failed");
            transaction.commit().await.expect("commit failed");
            let mut buffer = object.allocate_buffer(4096).await;
            buffer.fill(0xA7);
            let new_size = object.write_or_append(None, buffer.as_ref()).await.unwrap();
            assert_eq!(new_size, 4096);
        }

        // Remount the filesystem with the opposite barrier mode and write more data to our file.
        fs.close().await.expect("close failed");
        let device = fs.take_device().await;
        device.reopen(false);
        let fs = FxFilesystemBuilder::new()
            .barriers_enabled(!original_barrier_mode)
            .open(device)
            .await
            .expect("new filesystem failed");
        {
            let root_vol = root_volume(fs.clone()).await.expect("root_volume failed");
            let store = root_vol.volume("test").await.expect("opening test volume");
            let root_dir = Directory::open(&store, store.root_directory_object_id())
                .await
                .expect("open failed");
            let (object_id, _) =
                root_dir.lookup("file").await.expect("lookup failed").expect("missing file");
            let test_file = ObjectStore::open_object(&store, object_id, Default::default())
                .await
                .expect("open failed");
            // Write some more data.
            let mut buffer = test_file.allocate_buffer(4096).await;
            buffer.fill(0xA8);
            let new_size = test_file.write_or_append(None, buffer.as_ref()).await.unwrap();
            assert_eq!(new_size, 8192);
        }

        // Lastly, remount the filesystems with the original barrier mode and make sure everything
        // can be read from the file as expected.
        fs.close().await.expect("close failed");
        let device = fs.take_device().await;
        device.reopen(false);
        let fs = FxFilesystemBuilder::new()
            .barriers_enabled(original_barrier_mode)
            .open(device)
            .await
            .expect("new filesystem failed");
        {
            let root_vol = root_volume(fs.clone()).await.expect("root_volume failed");
            let store = root_vol.volume("test").await.expect("opening test volume");
            let root_dir = Directory::open(&store, store.root_directory_object_id())
                .await
                .expect("open failed");
            let (object_id, _) =
                root_dir.lookup("file").await.expect("lookup failed").expect("missing file");
            let test_file = ObjectStore::open_object(&store, object_id, Default::default())
                .await
                .expect("open failed");
            let mut buffer = test_file.allocate_buffer(8192).await;
            assert_eq!(
                test_file.read_aligned(0, buffer.as_mut()).await.expect("read failed"),
                8192,
                "short read"
            );
            let data = buffer.to_vec();
            assert_eq!(data[0..4096], [0xA7; 4096]);
            assert_eq!(data[4096..8192], [0xA8; 4096]);
        }
        fs.close().await.expect("close failed");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_read_only_mount_on_full_filesystem() {
        let device = DeviceHolder::new(FakeDevice::new(8192, TEST_DEVICE_BLOCK_SIZE));
        let fs =
            FxFilesystemBuilder::new().format(true).open(device).await.expect("new_empty failed");
        let root_store = fs.root_store();
        let root_directory = Directory::open(&root_store, root_store.root_directory_object_id())
            .await
            .expect("open failed");

        let mut transaction = fs
            .root_store()
            .new_transaction(
                lock_keys![LockKey::object(
                    root_store.store_object_id(),
                    root_directory.object_id()
                )],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        let handle = root_directory
            .create_child_file(&mut transaction, "test")
            .await
            .expect("create_child_file failed");
        transaction.commit().await.expect("commit failed");

        let mut buf = handle.allocate_buffer(4096).await;
        buf.fill(0xaa);
        loop {
            if handle.write_or_append(None, buf.as_ref()).await.is_err() {
                break;
            }
        }

        fs.close().await.expect("Close failed");

        // cladefs: the device is full, as nothing more could be written; upstream copies the part
        // of the device written to a device that size.
        let device = fs.take_device().await;
        device.reopen(true);
        let fs =
            FxFilesystemBuilder::new().read_only(true).open(device).await.expect("open failed");
        fs.close().await.expect("Close failed");
    }

    /// TASK-623 (board 2644): when `record_reaches` fails during `FxFilesystem::close` (e.g.
    /// `Hooks::on_pre_commit` failing the reach transaction), `close` still syncs the journal,
    /// stops background tasks, and calls `device().close()`, while keeping the reach pending in
    /// `unrecorded_reaches` and returning the error.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_close_still_closes_device_when_record_reaches_fails() {
        use crate::hooks::Hooks;
        use crate::object_store::transaction::{AllocatorMutation, Mutation};

        let (mut hooks, hooks_handle) = Hooks::new();
        let device = DeviceHolder::new(FakeDevice::new(8192, TEST_DEVICE_BLOCK_SIZE));
        let fs = FxFilesystemBuilder::new()
            .format(true)
            .hooks(hooks_handle)
            .open(device)
            .await
            .expect("new_empty failed");
        let root_store = fs.root_store();
        let root_dir = Directory::open(&root_store, root_store.root_directory_object_id())
            .await
            .expect("open root");
        let dir_lock =
            lock_keys![LockKey::object(root_store.store_object_id(), root_dir.object_id())];

        // 1. Create a file and write 1 block via `write_many` so the staging extent is Allocated.
        let mut txn =
            root_store.new_transaction(dir_lock.clone(), Options::default()).await.unwrap();
        let f1 = root_dir.create_child_file(&mut txn, "f1").await.unwrap();
        let f2 = root_dir.create_child_file(&mut txn, "f2").await.unwrap();
        txn.commit().await.unwrap();
        f1.write_many(&[(0, &[0x11; 4096])]).await.unwrap();

        // 2. Allocate in the shared staging extent and drop the transaction without committing so
        // `allocator.unrecorded_reaches` has a pending entry.
        {
            let mut dropped_txn =
                root_store.new_transaction(lock_keys![], Options::default()).await.unwrap();
            let _ = fs
                .allocator()
                .allocate_shared(&mut dropped_txn, root_store.store_object_id(), 4096)
                .await
                .unwrap();
        }
        assert!(
            fs.allocator().has_unrecorded_reaches(),
            "expected a pending reach from the dropped staging allocation"
        );

        // 3. Commit a real write after the last sync so we can verify `close` wrote the journal.
        f2.write_many(&[(0, &[0x22; 4096])]).await.unwrap();

        // 4. Fail `record_reaches`'s commit via `Hooks::on_pre_commit`.
        hooks.set_pre_commit(|txn| {
            let is_reach = txn.mutations().iter().any(|m| {
                matches!(m.mutation, Mutation::Allocator(AllocatorMutation::Reach { .. }))
            });
            if is_reach {
                anyhow::bail!(FxfsError::Internal);
            }
            Ok(())
        });

        let err = fs.close().await.expect_err("close must report the record_reaches failure");
        assert!(FxfsError::Internal.matches(&err), "{err:#}");
        // AC #2: the reach is still pending in `unrecorded_reaches`.
        assert!(
            fs.allocator().has_unrecorded_reaches(),
            "failed record_reaches commit must leave its reach pending"
        );

        // AC #1 & #3: `device().close()` was called (`root()` fails on a closed FakeDevice) and
        // the journal was written out before closing (f2's bytes survive reopen).
        drop(hooks);
        let device = fs.take_device().await;
        assert!(
            device.root().await.is_err(),
            "FakeDevice should already be closed by FxFilesystem::close"
        );
        device.reopen(false);
        let reopened = FxFilesystem::open(device).await.expect("reopen failed");
        let reopened_root = reopened.root_store();
        let reopened_dir =
            Directory::open(&reopened_root, reopened_root.root_directory_object_id())
                .await
                .unwrap();
        let (f2_id, _) = reopened_dir.lookup("f2").await.unwrap().expect("f2 missing");
        let reopened_f2 =
            ObjectStore::open_object(&reopened_root, f2_id, Default::default()).await.unwrap();
        assert_eq!(reopened_f2.contents(usize::MAX).await.unwrap()[..], [0x22; 4096]);
        reopened.close().await.unwrap();
    }

    /// board 11651 & board 7617: on an `atomic_commits() == true` device with `barriers_enabled`,
    /// dropped shared-extent allocations (`unrecorded_reaches`) must be journaled before sealing
    /// across:
    /// 1. `FxFilesystem::sync` with `holding_commits: true` (while `commit_mutex` is held — no
    ///    deadlock),
    /// 2. `Journal::write_super_block`, and
    /// 3. `Journal::flush` (`POST_BARRIER` flush),
    /// and a crash + remount must preserve the extent's reach and pass `fsck`.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_atomic_commits_records_reaches_across_holding_commits_sync_and_superblock() {
        use crate::fsck::{FsckOptions, fsck_with_options};

        let fake = FakeDevice::new(8192, TEST_DEVICE_BLOCK_SIZE).with_atomic_commits();
        let device = DeviceHolder::new(fake);
        let fs = FxFilesystemBuilder::new()
            .format(true)
            .barriers_enabled(true)
            .open(device)
            .await
            .expect("format failed");
        let root_store = fs.root_store();
        let root_dir = Directory::open(&root_store, root_store.root_directory_object_id())
            .await
            .expect("open root");
        let dir_lock =
            lock_keys![LockKey::object(root_store.store_object_id(), root_dir.object_id())];

        let mut txn =
            root_store.new_transaction(dir_lock.clone(), Options::default()).await.unwrap();
        let f1 = root_dir.create_child_file(&mut txn, "f1").await.unwrap();
        txn.commit().await.unwrap();
        // Establish a live shared staging extent.
        f1.write_many(&[(0, &[0x11; 4096])]).await.unwrap();

        let drop_shared_alloc = || async {
            let mut dropped =
                root_store.new_transaction(lock_keys![], Options::default()).await.unwrap();
            let range = fs
                .allocator()
                .allocate_shared(&mut dropped, root_store.store_object_id(), 4096)
                .await
                .unwrap();
            let device = fs.device();
            let mut buf = device.allocate_buffer(4096).await;
            buf.as_mut_slice().fill(0xaa);
            device.write(range.extent, range.range.start, buf.as_ref()).await.unwrap();
            drop(dropped);
            assert!(fs.allocator().has_unrecorded_reaches());
            range
        };

        // 1. `holding_commits: true` sync while `commit_mutex` is held must record reaches
        // without deadlocking `commit_mutex`.
        let range1 = drop_shared_alloc().await;
        {
            let _commits = fs.lock_commits().await;
            fs.sync(SyncOptions {
                flush_device: true,
                holding_commits: true,
                ..Default::default()
            })
            .await
            .expect("holding_commits sync failed");
        }
        assert!(
            !fs.allocator().has_unrecorded_reaches(),
            "holding_commits sync must record pending reaches"
        );

        // 2. `Journal::write_super_block` must record pending reaches before flushing/saving.
        f1.write_many(&[(4096, &[0x22; 4096])]).await.unwrap();
        let range2 = drop_shared_alloc().await;
        fs.journal().write_super_block().await.expect("write_super_block failed");
        assert!(
            !fs.allocator().has_unrecorded_reaches(),
            "write_super_block must record pending reaches"
        );

        // 3. `Journal::flush` (`POST_BARRIER`) must record reaches even when `Journal::sync` is
        // called directly (bypassing `FxFilesystem::sync`) on an `atomic_commits()` device.
        f1.write_many(&[(8192, &[0x33; 4096])]).await.unwrap();
        let range3 = drop_shared_alloc().await;
        fs.journal()
            .sync(SyncOptions { flush_device: true, ..Default::default() })
            .await
            .expect("journal POST_BARRIER sync failed");
        assert!(
            !fs.allocator().has_unrecorded_reaches(),
            "POST_BARRIER Journal::flush on atomic_commits device must record pending reaches"
        );

        // Simulate a crash right after the barrier and verify remounted allocator reach matches
        // what the device holds for all three shared staging extents.
        let crashed = fs.device().snapshot().unwrap();
        fs.close().await.unwrap();

        let reopened = FxFilesystemBuilder::new()
            .barriers_enabled(true)
            .open(crashed)
            .await
            .expect("remount after atomic crash failed");
        let fsck_opts = FsckOptions { fail_on_warning: true, ..Default::default() };
        fsck_with_options(reopened.clone(), &fsck_opts).await.expect("fsck failed");
        for range in [range1, range2, range3] {
            let size = reopened.allocator().extent_size(range.extent);
            assert!(
                size.is_some_and(|s| s >= range.range.end),
                "reopened allocator extent {:?} size {:?} fell short of recorded reach {}",
                range.extent,
                size,
                range.range.end
            );
            assert!(
                reopened.extent_counted(range.extent) >= range.range.end,
                "reopened counted bytes {} for {:?} fell short of recorded reach {}",
                reopened.extent_counted(range.extent),
                range.extent,
                range.range.end
            );
        }
        reopened.close().await.unwrap();
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn adv_atomic_commits_journal_sync_already_flushed_seals_unrecorded_reach() {
        use crate::fsck::{FsckOptions, fsck_with_options};

        let fake = FakeDevice::new(8192, TEST_DEVICE_BLOCK_SIZE).with_atomic_commits();
        let device = DeviceHolder::new(fake);
        let fs = FxFilesystemBuilder::new()
            .format(true)
            .barriers_enabled(true)
            .open(device)
            .await
            .expect("format failed");
        let root_store = fs.root_store();
        let root_dir = Directory::open(&root_store, root_store.root_directory_object_id())
            .await
            .expect("open root");
        let dir_lock =
            lock_keys![LockKey::object(root_store.store_object_id(), root_dir.object_id())];

        let mut txn =
            root_store.new_transaction(dir_lock.clone(), Options::default()).await.unwrap();
        let f1 = root_dir.create_child_file(&mut txn, "f1").await.unwrap();
        txn.commit().await.unwrap();
        f1.write_many(&[(0, &[0x11; 4096])]).await.unwrap();

        // Flush journal to device without a barrier so the journal has no unflushed bytes left.
        f1.write_many(&[(4096, &[0x22; 4096])]).await.unwrap();
        fs.journal().sync(SyncOptions { flush_device: false, ..Default::default() }).await.unwrap();
        fs.journal().wait_for_flush(fs.journal().file_offset()).await.unwrap();

        let range = {
            let mut dropped =
                root_store.new_transaction(lock_keys![], Options::default()).await.unwrap();
            let range = fs
                .allocator()
                .allocate_shared(&mut dropped, root_store.store_object_id(), 4096)
                .await
                .unwrap();
            let device = fs.device();
            let mut buf = device.allocate_buffer(4096).await;
            buf.as_mut_slice().fill(0xaa);
            device.write(range.extent, range.range.start, buf.as_ref()).await.unwrap();
            drop(dropped);
            range
        };
        assert!(fs.allocator().has_unrecorded_reaches());

        fs.journal()
            .sync(SyncOptions { flush_device: true, ..Default::default() })
            .await
            .expect("journal sync failed");

        let still_unrecorded = fs.allocator().has_unrecorded_reaches();
        let crashed = fs.device().snapshot().unwrap();
        fs.close().await.unwrap();

        let reopened = FxFilesystemBuilder::new()
            .barriers_enabled(true)
            .open(crashed)
            .await
            .expect("remount failed");
        let fsck_opts = FsckOptions { fail_on_warning: true, ..Default::default() };
        fsck_with_options(reopened.clone(), &fsck_opts).await.expect("fsck failed");
        let reopened_size = reopened.allocator().extent_size(range.extent);
        let reopened_counted = reopened.extent_counted(range.extent);
        reopened.close().await.unwrap();

        assert!(
            !still_unrecorded
                && reopened_size.is_some_and(|s| s >= range.range.end)
                && reopened_counted >= range.range.end,
            "sealed unrecorded reach! still_unrecorded={still_unrecorded}, \
             expected >= {}, reopened_size={reopened_size:?}, reopened_counted={reopened_counted}",
            range.range.end
        );
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn adv_atomic_commits_flush_for_frees_seals_unrecorded_reach() {
        use crate::fsck::{FsckOptions, fsck_with_options};

        let fake = FakeDevice::new(8192, TEST_DEVICE_BLOCK_SIZE).with_atomic_commits();
        let device = DeviceHolder::new(fake);
        let fs = FxFilesystemBuilder::new()
            .format(true)
            .barriers_enabled(true)
            .open(device)
            .await
            .expect("format failed");
        let root_store = fs.root_store();
        let root_dir = Directory::open(&root_store, root_store.root_directory_object_id())
            .await
            .expect("open root");
        let dir_lock =
            lock_keys![LockKey::object(root_store.store_object_id(), root_dir.object_id())];
        let mut txn = root_store.new_transaction(dir_lock, Options::default()).await.unwrap();
        let f1 = root_dir.create_child_file(&mut txn, "f1").await.unwrap();
        txn.commit().await.unwrap();
        f1.write_many(&[(0, &[0x11; 4096])]).await.unwrap();
        fs.journal().sync(SyncOptions { flush_device: false, ..Default::default() }).await.unwrap();
        let offset = fs.journal().file_offset();
        fs.journal().wait_for_flush(offset).await.unwrap();

        let range = {
            let mut dropped =
                root_store.new_transaction(lock_keys![], Options::default()).await.unwrap();
            let range = fs
                .allocator()
                .allocate_shared(&mut dropped, root_store.store_object_id(), 4096)
                .await
                .unwrap();
            let dev = fs.device();
            let mut buf = dev.allocate_buffer(4096).await;
            buf.as_mut_slice().fill(0xaa);
            dev.write(range.extent, range.range.start, buf.as_ref()).await.unwrap();
            range
        };
        assert!(fs.allocator().has_unrecorded_reaches());

        fs.journal().flush_for_frees().await.expect("flush_for_frees failed");

        let crashed = fs.device().snapshot().unwrap();
        let still_unrecorded = fs.allocator().has_unrecorded_reaches();
        fs.close().await.unwrap();

        let reopened = FxFilesystemBuilder::new()
            .barriers_enabled(true)
            .open(crashed)
            .await
            .expect("remount after atomic crash failed");
        let fsck_opts = FsckOptions { fail_on_warning: true, ..Default::default() };
        fsck_with_options(reopened.clone(), &fsck_opts).await.expect("fsck failed");
        let size = reopened.allocator().extent_size(range.extent);
        let counted = reopened.extent_counted(range.extent);
        reopened.close().await.unwrap();
        assert!(
            !still_unrecorded
                && size.is_some_and(|s| s >= range.range.end)
                && counted >= range.range.end,
            "Journal::flush_for_frees sealed unrecorded reach without journaling it: \
             still_unrecorded={still_unrecorded}, extent={:?}, expected_end={}, \
             reopened_size={size:?}, reopened_counted={counted}",
            range.extent,
            range.range.end,
        );
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn adv_record_reaches_for_flush_deadlocks_commit_mutex_after_fsck() {
        use crate::object_store::journal::{JournalOptions, LazyFlush};
        use futures::FutureExt as _;
        use std::time::Duration;

        let fake = FakeDevice::new(8192, TEST_DEVICE_BLOCK_SIZE);
        let device = DeviceHolder::new(fake);
        let fs = FxFilesystemBuilder::new()
            .format(true)
            .journal_options(JournalOptions {
                barriers_enabled: true,
                lazy_flush: Some(LazyFlush { bytes: 1 << 20, delay: Duration::from_millis(100) }),
                ..Default::default()
            })
            .open(device)
            .await
            .expect("format failed");
        let root_store = fs.root_store();
        let root_dir = Directory::open(&root_store, root_store.root_directory_object_id())
            .await
            .expect("open root");
        let dir_lock =
            lock_keys![LockKey::object(root_store.store_object_id(), root_dir.object_id())];

        let mut txn =
            root_store.new_transaction(dir_lock.clone(), Options::default()).await.unwrap();
        let f1 = root_dir.create_child_file(&mut txn, "f1").await.unwrap();
        txn.commit().await.unwrap();
        f1.write_many(&[(0, &[0x11; 4096])]).await.unwrap();
        fs.sync(SyncOptions { flush_device: true, ..Default::default() }).await.unwrap();

        let mut writes = Vec::new();
        for i in 1..=64u64 {
            writes.push((i * 4096, [i as u8; 4096]));
        }
        for (off, buf) in &writes {
            f1.write_many(&[(*off, &buf[..])]).await.unwrap();
        }
        {
            let mut dropped =
                root_store.new_transaction(lock_keys![], Options::default()).await.unwrap();
            let range = fs
                .allocator()
                .allocate_shared(&mut dropped, root_store.store_object_id(), 4096)
                .await
                .unwrap();
            let device = fs.device();
            let mut buf = device.allocate_buffer(4096).await;
            buf.as_mut_slice().fill(0xaa);
            device.write(range.extent, range.range.start, buf.as_ref()).await.unwrap();
        }
        assert!(fs.allocator().has_unrecorded_reaches());

        let fsck_guard = fs.lock_commits_for_fsck().await;

        let fs_a = fs.clone();
        let caller_a = fasync::Task::spawn(async move {
            let _commits = fs_a.lock_commits().await;
            fs_a.sync(SyncOptions {
                flush_device: true,
                holding_commits: true,
                ..Default::default()
            })
            .await
        });

        fasync::Timer::new(Duration::from_millis(250)).await;
        drop(fsck_guard);

        let result = futures::select! {
            res = caller_a.fuse() => Ok(res),
            _ = fasync::Timer::new(Duration::from_secs(2)).fuse() => Err("AB-BA deadlock"),
        };
        assert!(
            result.is_ok(),
            "DEADLOCK: holding_commits sync timed out after 2s waiting on flush_task"
        );
        result.unwrap().expect("holding_commits sync failed");
        fs.close().await.unwrap();
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn adv_holding_commits_sync_deadlocks_during_no_lock_fsck() {
        use crate::fsck::{FsckOptions, fsck_with_options};
        use futures::future::{Either, select};
        use storage_device::fake_device::RoundTrips;

        let trips = Arc::new(RoundTrips::default());
        let fake = FakeDevice::new(8192, TEST_DEVICE_BLOCK_SIZE)
            .with_atomic_commits()
            .with_round_trips(trips);
        let device = DeviceHolder::new(fake);
        let fs = FxFilesystemBuilder::new()
            .format(true)
            .barriers_enabled(true)
            .open(device)
            .await
            .expect("format failed");
        let root_store = fs.root_store();
        let root_dir = Directory::open(&root_store, root_store.root_directory_object_id())
            .await
            .expect("open root");
        let dir_lock =
            lock_keys![LockKey::object(root_store.store_object_id(), root_dir.object_id())];
        let mut txn = root_store.new_transaction(dir_lock, Options::default()).await.unwrap();
        let f1 = root_dir.create_child_file(&mut txn, "f1").await.unwrap();
        txn.commit().await.unwrap();
        f1.write_many(&[(0, &[0x11; 4096])]).await.unwrap();
        fs.journal().force_compact().await.unwrap();
        f1.write_many(&[(4096, &[0x22; 4096])]).await.unwrap();

        let _commits = fs.lock_commits().await;
        let fsck_opts = FsckOptions { no_lock: true, quiet: true, ..Default::default() };
        let mut fsck_fut = Box::pin(fsck_with_options(fs.clone(), &fsck_opts));
        assert!(
            futures::poll!(&mut fsck_fut).is_pending(),
            "no_lock fsck should yield on async device read"
        );

        {
            let mut dropped =
                root_store.new_transaction(lock_keys![], Options::default()).await.unwrap();
            let range = fs
                .allocator()
                .allocate_shared(&mut dropped, root_store.store_object_id(), 4096)
                .await
                .unwrap();
            let dev = fs.device();
            let mut buf = dev.allocate_buffer(4096).await;
            buf.as_mut_slice().fill(0xaa);
            dev.write(range.extent, range.range.start, buf.as_ref()).await.unwrap();
        }
        assert!(fs.allocator().has_unrecorded_reaches());

        let mut sync_fut = Box::pin(fs.journal().sync(SyncOptions {
            flush_device: true,
            holding_commits: true,
            ..Default::default()
        }));
        let wait_park = Box::pin(fuchsia_async::Timer::new(
            std::time::Instant::now() + std::time::Duration::from_millis(50),
        ));
        if let Either::Left((res, _)) = select(&mut sync_fut, wait_park).await {
            res.expect("sync failed");
            fsck_fut.await.expect("fsck failed");
            drop(_commits);
            fs.close().await.unwrap();
            return;
        }

        fsck_fut.await.expect("fsck failed");

        let timer = Box::pin(fuchsia_async::Timer::new(
            std::time::Instant::now() + std::time::Duration::from_secs(2),
        ));
        match select(sync_fut, timer).await {
            Either::Left((res, _)) => {
                res.expect("sync failed");
            }
            Either::Right(_) => {
                panic!("DEADLOCK: holding_commits sync + no_lock fsck deadlocked flush_task");
            }
        }
        drop(_commits);
        fs.close().await.unwrap();
    }
}
