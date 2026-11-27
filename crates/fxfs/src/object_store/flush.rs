// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Fuchsia's `src/storage/fxfs/src/object_store/flush.rs`. cladefs snapshots (volume snapshots that
// share their source's layers and extents) deviate from upstream here: compaction releases a
// store's old layers by reference count, as snapshots share them, and tombstones only those no
// store references. A store's base layers (backlog doc-10)
// stay at the bottom of its tree, in no compaction, and in no reservation.

// This module is responsible for flushing (a.k.a. compacting) the object store trees.

use crate::filesystem::{FlushReason, ForceMajor};
use crate::lsm_tree::types::{ItemRef, LayerIterator};
use crate::lsm_tree::{KeptContents, KeptWriter, LSMTree, layer_from_written_handle};
use crate::object_handle::{ObjectHandle, ReadObjectHandle};
use crate::object_store::extent_record::ExtentValue;
use crate::object_store::object_manager::{ObjectManager, ReservationUpdate};
use crate::object_store::object_record::{ObjectKey, ObjectValue};
use crate::object_store::transaction::{AssociatedObject, LockKey, LockKeys, Mutation, lock_keys};
use crate::object_store::{
    AssocObj, DirectWriter, HandleOptions, ObjectStore, Options, StoreInfo, tree,
};
use crate::serialized_types::{LATEST_VERSION, Version};
use anyhow::{Context, Error};
use std::sync::OnceLock;

/// cladefs: the largest layer a compaction keeps what it wrote of, cached (`KeptWriter`): as much
/// as a few journals' worth of changes merged.
const KEPT_LAYER_MAX: usize = 16 << 20;

impl ObjectStore {
    /// Takes a flush lock on self and performs the flush with a default
    /// FlushReason::Journal(ForceMajor::False).
    pub async fn flush(&self) -> Result<Version, Error> {
        self.flush_with_reason(FlushReason::Journal(ForceMajor::False)).await
    }

    /// Takes a flush lock on self and performs the flush.
    pub async fn flush_with_reason(&self, reason: FlushReason) -> Result<Version, Error> {
        let filesystem = self.filesystem();

        let keys = lock_keys![LockKey::flush(self.store_object_id())];
        let _guard = filesystem.lock_manager().write_lock(keys).await;

        self.flush_guarded_with_reason(reason).await
    }

    /// Performs a flush while already holding a flush guard on self.
    pub async fn flush_guarded_with_reason(&self, reason: FlushReason) -> Result<Version, Error> {
        if self.parent_store.is_none() {
            // Early exit, but still return the earliest version used by a struct in the tree
            return Ok(self.tree.get_earliest_version());
        }

        // After taking the lock, check to see if the store has been deleted.
        if self.is_deleted() {
            // When we compact, it's possible that the store has been deleted since we gathered the
            // list of stores that need compacting.  This is benign.
            return Ok(LATEST_VERSION);
        }

        let filesystem = self.filesystem();
        let object_manager = filesystem.object_manager();
        let earliest_version = self.tree.get_earliest_version();
        let needs_flush = object_manager.needs_flush(self.store_object_id);
        // If we don't need to do anything for the stated flush purpose, do nothing.
        if reason == FlushReason::Journal(ForceMajor::False) && !needs_flush {
            // Early exit, but still return the earliest version used by a struct in the
            // tree.
            return Ok(earliest_version);
        }

        // cladefs: box flush stages in non-inlined frames to fit 16 KiB guest kernel stacks
        // (TASK-26.9).
        crate::boxed(|| self.flush_layers(reason))
            .await
            .with_context(|| format!("Failed to flush object store {}", self.store_object_id))?;

        if let Some(callback) = &*self.flush_callback.lock() {
            callback(self);
        }

        // Return the earliest version used by a struct in the tree
        Ok(self.tree.get_earliest_version())
    }

    // Flushes the store's tree into a new layer. Returns the layer file sizes.
    async fn flush_layers(&self, reason: FlushReason) -> Result<Vec<u64>, Error> {
        struct StoreInfoSnapshot<'a> {
            store: &'a ObjectStore,
            store_info: OnceLock<StoreInfo>,
        }
        impl AssociatedObject for StoreInfoSnapshot<'_> {
            fn will_apply_mutation(
                &self,
                _mutation: &Mutation,
                _object_id: u64,
                _manager: &ObjectManager,
            ) {
                self.store_info.set(self.store.store_info()).unwrap();
            }
        }

        let store_info_snapshot = StoreInfoSnapshot { store: self, store_info: OnceLock::new() };

        let filesystem = self.filesystem();
        let object_manager = filesystem.object_manager();
        let reservation = object_manager.metadata_reservation();
        let txn_options = Options {
            skip_journal_checks: true,
            borrow_metadata_space: true,
            allocator_reservation: Some(reservation),
            ..Default::default()
        };

        // The BeginFlush mutation must be within a transaction that has no impact on StoreInfo
        // since we want to get an accurate snapshot of StoreInfo.
        // cladefs: box setup/teardown transaction calls so `flush_layers` stays small on
        // 16 KiB guest kernel stacks (TASK-26.9).
        let mut transaction =
            crate::boxed(|| self.new_transaction(lock_keys![], txn_options)).await?;
        transaction.add_with_object(
            self.store_object_id(),
            Mutation::BeginFlush,
            AssocObj::Borrowed(&store_info_snapshot),
        );
        transaction.commit().await.context("Failed to commit BeginFlush transaction")?;

        let mut new_store_info = store_info_snapshot.store_info.into_inner().unwrap();

        // There is a transaction to create objects at the start and then another transaction at the
        // end. Between those two transactions, there are transactions that write to the files.  In
        // the first transaction, objects are created in the graveyard. Upon success, the objects
        // are removed from the graveyard.
        // cladefs: box setup/teardown transaction calls so `flush_layers` stays small on
        // 16 KiB guest kernel stacks (TASK-26.9).
        let mut transaction =
            crate::boxed(|| self.new_transaction(lock_keys![], txn_options)).await?;

        // Create and write a new layer, compacting existing layers.
        let parent_store = self.parent_store.as_ref().unwrap();
        let handle_options = HandleOptions { skip_journal_checks: true, ..Default::default() };
        // cladefs: box object creation so `flush_layers` stays small on 16 KiB stacks (TASK-26.9).
        let new_object_tree_layer = crate::boxed(|| {
            ObjectStore::create_object(parent_store, &mut transaction, handle_options)
        })
        .await?;
        // cladefs: keeping what it writes, which the new layer caches rather than reads back.
        let kept = KeptContents::default();
        let writer = KeptWriter::new(
            DirectWriter::new(&new_object_tree_layer, txn_options).await,
            KEPT_LAYER_MAX,
            kept.clone(),
        );
        let new_object_tree_layer_object_id = new_object_tree_layer.object_id();
        parent_store.add_to_graveyard(&mut transaction, new_object_tree_layer_object_id);

        // cladefs: box `commit` so `flush_layers` stays small on 16 KiB stacks (TASK-26.9).
        crate::boxed(|| transaction.commit())
            .await
            .context("Failed to commit create layer transaction")?;

        // *Do* the actual compaction.
        let full_compaction = matches!(reason, FlushReason::Journal(ForceMajor::True));
        // cladefs: box `tree::flush` and `layer_from_written_handle` in non-inlined frames so
        // `flush_layers` fits in 16 KiB guest kernel stacks (TASK-26.9).
        let (layers_to_keep, old_layers) = crate::boxed(|| {
            tree::flush(
                &self.tree,
                writer,
                matches!(reason, FlushReason::Journal(_))
                    .then(|| filesystem.journal().get_compaction_yielder()),
                full_compaction,
            )
        })
        .await
        .context("Failed to flush tree")?;

        // Finalise the compaction.
        let mut new_layers =
            vec![crate::boxed(|| layer_from_written_handle(new_object_tree_layer, &kept)).await?];
        new_layers.extend(layers_to_keep.iter().map(|l| (*l).clone()));

        new_store_info.layers = Vec::new();
        for layer in &new_layers {
            if let Some(handle) = layer.handle() {
                new_store_info.layers.push(handle.object_id());
            }
        }

        let reservation_update: ReservationUpdate; // Must live longer than end_transaction.
        let parent_store_object_id = self.parent_store.as_ref().unwrap().store_object_id();
        // cladefs: the old layers are locked, as other stores (snapshots) may share them, and release
        // them too.
        let old_layer_object_ids: Vec<u64> =
            old_layers.iter().filter_map(|layer| layer.handle().map(|h| h.object_id())).collect();
        let mut end_keys = vec![LockKey::object(
            parent_store_object_id,
            self.store_info_handle_object_id().unwrap(),
        )];
        end_keys.extend(
            old_layer_object_ids.iter().map(|id| LockKey::object(parent_store_object_id, *id)),
        );
        // cladefs: box `new_transaction` so `flush_layers` stays small on 16 KiB stacks
        // (TASK-26.9).
        let mut end_transaction =
            crate::boxed(|| parent_store.new_transaction(LockKeys::Vec(end_keys), txn_options))
                .await?;

        parent_store.remove_from_graveyard(&mut end_transaction, new_object_tree_layer_object_id);

        // Release the existing layers we're compacting: those no other store shares move to the
        // graveyard at the end. cladefs: a layer is shared by the stores a snapshot made, which hold
        // references to it; upstream, a store's layers are its own, and all move to the graveyard.
        let mut released_layer_object_ids = Vec::new();
        for object_id in old_layer_object_ids {
            // cladefs: box `adjust_refs` so `flush_layers` stays small on 16 KiB stacks
            // (TASK-26.9).
            if crate::boxed(|| parent_store.adjust_refs(&mut end_transaction, object_id, -1))
                .await?
            {
                released_layer_object_ids.push(object_id);
            }
        }

        // `last_object_id` is updated differently to other members of `StoreInfo`: it must match
        // the current in-memory value.
        //
        // This might capture object IDs that might be in transactions not yet committed.  In
        // theory, we could do better than this but it's not worth the effort.
        new_store_info.last_object_id = *self.last_object_id.lock();

        // cladefs: box `write_store_info` and `commit_with_callback` so `flush_unlocked` stays
        // small on 16 KiB guest kernel stacks (TASK-26.9).
        crate::boxed(|| self.write_store_info(&mut end_transaction, &new_store_info)).await?;

        let layer_file_sizes = new_layers
            .iter()
            .map(|l| l.handle().map(ReadObjectHandle::get_size).unwrap_or(0))
            .collect::<Vec<u64>>();

        let total_layer_size = layer_file_sizes.iter().sum();
        reservation_update =
            ReservationUpdate::new(tree::reservation_amount_from_layer_size(total_layer_size));

        end_transaction.add_with_object(
            self.store_object_id(),
            Mutation::EndFlush,
            AssocObj::Borrowed(&reservation_update),
        );

        crate::boxed(|| {
            end_transaction.commit_with_callback(|_| {
                self.store_info.lock().layers = new_store_info.layers;
                self.tree.set_layers(new_layers);
            })
        })
        .await
        .context("Failed to commit EndFlush transaction")?;

        // Now close the layers and purge those released.
        for layer in old_layers {
            let object_id = layer.handle().map(|h| h.object_id());
            layer.close_layer().await;
            if let Some(object_id) = object_id
                && released_layer_object_ids.contains(&object_id)
            {
                parent_store
                    .tombstone_object(object_id, txn_options, None)
                    .await
                    .context("Failed to tombstone old layer")?;
            }
        }

        Ok(layer_file_sizes)
    }
}

#[cfg(test)]
mod tests {
    use super::{FlushReason, ForceMajor};
    use crate::filesystem::{FxFilesystem, FxFilesystemBuilder};
    use crate::object_handle::{ObjectHandle, ReadObjectHandle, WriteObjectHandle};
    use crate::object_store::directory::Directory;
    use crate::object_store::journal::JournalOptions;
    use crate::object_store::transaction::{Options, lock_keys};
    use crate::object_store::volume::root_volume;
    use crate::object_store::{HandleOptions, LockKey, NewChildStoreOptions, ObjectStore};
    use storage_device::DeviceHolder;
    use storage_device::fake_device::FakeDevice;

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_major_compaction_frees_reservation_and_merges_layers() {
        let device = DeviceHolder::new(FakeDevice::new(32768, 512));
        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");
        let root_volume = root_volume(fs.clone()).await.expect("root_volume failed");
        let store = root_volume
            .new_volume("test", NewChildStoreOptions::default())
            .await
            .expect("new_volume failed");
        let root_dir =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");

        // 1. Populate the store with files to create an initial layer > 512 KiB
        // (DEFAULT_RECLAIM_SIZE).
        let num_files = 2000;
        let mut file_ids = Vec::with_capacity(num_files);
        for i in 0..num_files {
            let filename = format!("file_{:04}_{:<200}", i, i);
            let mut transaction = store
                .new_transaction(
                    lock_keys![LockKey::object(store.store_object_id(), root_dir.object_id())],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            let file = root_dir
                .create_child_file(&mut transaction, &filename)
                .await
                .expect("create_child_file failed");
            transaction.commit().await.expect("commit failed");
            file_ids.push((file.object_id(), filename));
        }

        // Flush (minor) to create Layer 0.
        store.flush().await.expect("flush failed");

        let info_initial = store.load_store_info().await.expect("load_store_info failed");
        assert_eq!(info_initial.layers.len(), 1);
        let initial_reservation = fs
            .object_manager()
            .reservation(store.store_object_id())
            .expect("reservation not found");

        // 2. Delete half the files (1000 files).
        for (oid, filename) in &file_ids[0..1000] {
            let mut transaction = store
                .new_transaction(
                    lock_keys![
                        LockKey::object(store.store_object_id(), root_dir.object_id()),
                        LockKey::object(store.store_object_id(), *oid),
                    ],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            let replaced = crate::object_store::directory::replace_child(
                &mut transaction,
                None,
                (&root_dir, filename.as_str()),
            )
            .await
            .expect("replace_child failed");
            assert_matches::assert_matches!(
                replaced,
                crate::object_store::directory::ReplacedChild::Object(id) if id == *oid
            );
            transaction.commit().await.expect("commit failed");
        }

        // 3. Minor flush after deleting 1000 files (FlushReason::Journal(ForceMajor::False)).
        // Minor compaction will NOT merge with the base layer because base layer > 512 KiB.
        store
            .flush_with_reason(FlushReason::Journal(ForceMajor::False))
            .await
            .expect("minor flush failed");

        let info_after_minor = store.load_store_info().await.expect("load_store_info failed");
        assert_eq!(
            info_after_minor.layers.len(),
            2,
            "Minor compaction should keep the base layer, creating 2 layers"
        );
        let res_after_minor = fs
            .object_manager()
            .reservation(store.store_object_id())
            .expect("reservation not found");
        assert!(
            res_after_minor >= initial_reservation,
            "Minor compaction should not decrease reservation because base layer is kept"
        );

        // 4. Major flush (FlushReason::Journal(ForceMajor::True)).
        // Major compaction must merge ALL layers, purge tombstones, and reduce reservation.
        store
            .flush_with_reason(FlushReason::Journal(ForceMajor::True))
            .await
            .expect("major flush failed");

        let info_after_major = store.load_store_info().await.expect("load_store_info failed");
        assert_eq!(
            info_after_major.layers.len(),
            1,
            "Major compaction should merge all layers into 1"
        );
        let res_after_major = fs
            .object_manager()
            .reservation(store.store_object_id())
            .expect("reservation not found");
        assert!(
            res_after_major < initial_reservation,
            "Major compaction should decrease reservation (was {}, initial was {})",
            res_after_major,
            initial_reservation
        );

        // 5. Verify data integrity.
        // Deleted files must be gone.
        for (oid, filename) in &file_ids[0..1000] {
            assert!(
                root_dir.lookup(filename).await.expect("lookup failed").is_none(),
                "Deleted file {} should not exist",
                oid
            );
        }
        for (oid, filename) in &file_ids[1000..num_files] {
            let res = root_dir.lookup(filename).await.expect("lookup failed");
            assert!(res.is_some(), "Surviving file {} should exist", oid);
            assert_eq!(res.unwrap().0, *oid);
        }

        fs.close().await.expect("close failed");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_major_compaction_without_journal_mutations() {
        let device = DeviceHolder::new(FakeDevice::new(32768, 512));
        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");
        let root_volume = root_volume(fs.clone()).await.expect("root_volume failed");
        let store = root_volume
            .new_volume("test", NewChildStoreOptions::default())
            .await
            .expect("new_volume failed");
        let root_dir =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");

        // Create initial base layer > 512 KiB.
        for i in 0..2000 {
            let filename = format!("file_{:04}_{:<200}", i, i);
            let mut transaction = store
                .new_transaction(
                    lock_keys![LockKey::object(store.store_object_id(), root_dir.object_id())],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            root_dir.create_child_file(&mut transaction, &filename).await.expect("create failed");
            transaction.commit().await.expect("commit failed");
        }
        store.flush().await.expect("flush failed");

        // Write a small file and do a minor flush to create a second layer.
        let mut transaction = store
            .new_transaction(
                lock_keys![LockKey::object(store.store_object_id(), root_dir.object_id())],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        root_dir.create_child_file(&mut transaction, "extra_file").await.expect("create failed");
        transaction.commit().await.expect("commit failed");
        store
            .flush_with_reason(FlushReason::Journal(ForceMajor::False))
            .await
            .expect("flush failed");

        let info = store.load_store_info().await.expect("load_store_info failed");
        assert_eq!(info.layers.len(), 2, "Should have 2 layers before major flush");

        // Now, do a major compaction when there are NO journal mutations in ObjectManager.
        assert!(!fs.object_manager().needs_flush(store.store_object_id()));
        store
            .flush_with_reason(FlushReason::Journal(ForceMajor::True))
            .await
            .expect("major flush failed");

        let info_after_major = store.load_store_info().await.expect("load_store_info failed");
        assert_eq!(
            info_after_major.layers.len(),
            1,
            "Major flush should merge 2 layers into 1 even without journal mutations"
        );

        // Doing another major flush when already at 1 layer and no mutations should succeed
        // and leave 1 layer.
        assert!(!fs.object_manager().needs_flush(store.store_object_id()));
        store
            .flush_with_reason(FlushReason::Journal(ForceMajor::True))
            .await
            .expect("second major flush failed");
        let info_after_second = store.load_store_info().await.expect("load_store_info failed");
        assert_eq!(
            info_after_second.layers.len(),
            1,
            "Major flush when at 1 layer should succeed and leave 1 layer"
        );

        fs.close().await.expect("close failed");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_major_compaction_purges_deleted_extents() {
        let device = DeviceHolder::new(FakeDevice::new(32768, 512));
        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");
        let root_volume = root_volume(fs.clone()).await.expect("root_volume failed");
        let store = root_volume
            .new_volume("test", NewChildStoreOptions::default())
            .await
            .expect("new_volume failed");
        let root_dir =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");

        // 1. Create a base layer > 512 KiB so minor compaction won't merge with it.
        for i in 0..2000 {
            let filename = format!("file_{:04}_{:<200}", i, i);
            let mut transaction = store
                .new_transaction(
                    lock_keys![LockKey::object(store.store_object_id(), root_dir.object_id())],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            root_dir.create_child_file(&mut transaction, &filename).await.expect("create failed");
            transaction.commit().await.expect("commit failed");
        }

        // Create a data file with extents.
        let data_oid = {
            let mut transaction = store
                .new_transaction(
                    lock_keys![LockKey::object(store.store_object_id(), root_dir.object_id())],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            let file = root_dir
                .create_child_file(&mut transaction, "data_file")
                .await
                .expect("create_child_file failed");
            let oid = file.object_id();
            transaction.commit().await.expect("commit failed");
            oid
        };

        let data_file = ObjectStore::open_object(&store, data_oid, HandleOptions::default())
            .await
            .expect("open_object failed");
        {
            let mut transaction = store
                .new_transaction(
                    lock_keys![LockKey::object(store.store_object_id(), data_oid)],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            let mut buffer = data_file.allocate_buffer(131072).await;
            buffer.fill(0xAB);
            data_file
                .txn_write(&mut transaction, 0, buffer.as_ref())
                .await
                .expect("txn_write failed");
            transaction.commit().await.expect("commit failed");
        }

        // Flush (minor) to commit extents to Layer 0.
        store.flush().await.expect("flush failed");

        let info0 = store.load_store_info().await.expect("load_store_info failed");
        assert_eq!(info0.layers.len(), 1);

        // Truncate data_file to 0 to write extent tombstones (ExtentValue::None).
        data_file.truncate(0).await.expect("truncate failed");

        // Minor flush: writes Layer 1 with extent tombstones on top of Layer 0.
        store
            .flush_with_reason(FlushReason::Journal(ForceMajor::False))
            .await
            .expect("minor flush failed");
        let info1 = store.load_store_info().await.expect("load_store_info failed");
        assert_eq!(info1.layers.len(), 2, "Minor flush should keep Layer 0 and create Layer 1");

        // Major flush: purges ExtentValue::None tombstones via major_iter and merges into 1 layer.
        store
            .flush_with_reason(FlushReason::Journal(ForceMajor::True))
            .await
            .expect("major flush failed");
        let info2 = store.load_store_info().await.expect("load_store_info failed");
        assert_eq!(info2.layers.len(), 1, "Major flush should merge into 1 layer");

        // Verify data_file size is 0.
        assert_eq!(data_file.get_size(), 0);

        fs.close().await.expect("close failed");
    }

    // This test case is a regression test for b/548631578.  It verifies that we will force major
    // compactions when borrowed_metadata_space gets too large relative to metadata_reservation.
    // Minor compactions are not sufficient in all cases to return borrowed metadata space, which
    // can eventually exhuast the metadata reservation and prevent all operations (including
    // compaction itself).
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_borrow_metadata_space_fails_without_major_compaction() {
        let reclaim_size = 65536;
        // 102,400 blocks of 512 bytes = 50 MiB device.
        let device = DeviceHolder::new(FakeDevice::new(102400, 512));
        let fs = FxFilesystemBuilder::new()
            .journal_options(JournalOptions { reclaim_size, ..Default::default() })
            .format(true)
            .open(device)
            .await
            .expect("open failed");

        // The test creates many files in several stores, and then deletes them.  The deletions
        // create additional layer files, which should be collapsed into the base layers (and
        // cancel out the file creations) following a major compaction, which should happen
        // automatically due to the large amount of borrowed space.
        let root_volume = root_volume(fs.clone()).await.expect("root_volume failed");
        let num_stores = 3;
        let files_per_store = 3500;
        let mut stores_and_files = Vec::new();

        for s in 0..num_stores {
            let store = root_volume
                .new_volume(&format!("test_{s}"), NewChildStoreOptions::default())
                .await
                .expect("new_volume failed");
            let root_dir = Directory::open(&store, store.root_directory_object_id())
                .await
                .expect("open failed");
            let mut file_ids = Vec::with_capacity(files_per_store);
            for i in 0..files_per_store {
                let filename = format!("file_{:04}_{:<200}", i, i);
                let mut transaction = store
                    .new_transaction(
                        lock_keys![LockKey::object(store.store_object_id(), root_dir.object_id())],
                        Options::default(),
                    )
                    .await
                    .expect("new_transaction failed");
                let file = root_dir
                    .create_child_file(&mut transaction, &filename)
                    .await
                    .expect("create_child_file failed");
                transaction.commit().await.expect("commit failed");
                file_ids.push((file.object_id(), filename));
            }
            store
                .flush_with_reason(FlushReason::Journal(ForceMajor::True))
                .await
                .expect("flush failed");
            stores_and_files.push((store, root_dir, file_ids));
        }

        for (_, root_dir, file_ids) in stores_and_files {
            for (oid, filename) in &file_ids[0..2500] {
                let mut context = root_dir
                    .acquire_context_for_replace(None, filename.as_str(), true)
                    .await
                    .expect("acquire_context failed");
                let replaced = crate::object_store::directory::replace_child(
                    &mut context.transaction,
                    None,
                    (&root_dir, filename.as_str()),
                )
                .await
                .expect("replace_child failed");
                assert_matches::assert_matches!(
                    replaced,
                    crate::object_store::directory::ReplacedChild::Object(id) if id == *oid
                );
                context.transaction.commit().await.expect("commit failed");
            }
        }

        fs.close().await.expect("close failed");
    }
}

impl tree::MajorCompactable<ObjectKey, ObjectValue> for LSMTree<ObjectKey, ObjectValue> {
    async fn major_iter(
        iter: impl LayerIterator<ObjectKey, ObjectValue>,
    ) -> Result<impl LayerIterator<ObjectKey, ObjectValue>, Error> {
        iter.filter(|item: ItemRef<'_, _, _>| match item {
            // Object Tombstone.
            ItemRef { value: ObjectValue::None, .. } => false,
            // Deleted extent.
            ItemRef { value: ObjectValue::Extent(ExtentValue::None), .. } => false,
            _ => true,
        })
        .await
    }
}
