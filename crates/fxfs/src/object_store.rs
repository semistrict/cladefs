// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Fuchsia's `src/storage/fxfs/src/object_store.rs`. Volumes are not installed from images within
// files (no `install`, `extent_mapping_iterator`): such an image is a block device's, which
// cladefs's extent store has none of (backlog doc-4). `dirty_reservation` reserves room for the data a page cache
// holds dirty as it dirties it, as Fuchsia's pager-backed files do in its platform crate, for the
// guest kernel's page cache (TASK-80). A store's information still has the fields of the layers
// of a base, a volume another filesystem wrote, below its own (`StoreInfo::base_layers`), which a
// frozen format keeps: shared volumes are removed (board 14527), and a store naming base layers
// is refused on opening. What stops
// a transaction is what it takes of the journal (`Transaction::room`), not the number of mutations
// upstream's TRANSACTION_MUTATION_THRESHOLD stood for it with. An unlinked object is tombstoned in
// a transaction of its caller's by `ObjectStore::txn_tombstone_unlinked` alone, which the
// graveyard's batches (`tombstone_objects`), purging unlinks and fxfs-posix's queued unlinks call.
// No store is encrypted, nor any object, as Fxfs encrypts nothing (cladefs encrypts below it, in
// its extents and buckets): no keys, key manager or lock states, and a store's mutations go to the
// journal as they are.

pub mod allocator;
pub mod base_layer;
pub mod caching_object_handle;
pub mod data_object_handle;
pub mod directory;
pub mod dirty_reservation;
pub mod extent;
mod extent_record;
pub mod flush;
use crate::filesystem::FlushReason;
pub mod graveyard;
pub mod journal;
pub(crate) mod merge;
pub mod object_manager;
pub mod object_record;
mod store_object_handle;
pub mod transaction;
mod tree;
pub(crate) mod tree_cache;
pub mod volume;

pub use data_object_handle::{
    DataObjectHandle, DirectWriter, ExtentAccess, FileExtent, WriteAttrs,
};
pub use directory::Directory;
pub use object_record::{ChildValue, ObjectDescriptor, PosixAttributes, SpecialFile, Timestamp};
pub use store_object_handle::{
    MAX_INLINE_XATTR_SIZE, MAX_XATTR_NAME_SIZE, MAX_XATTR_VALUE_SIZE, SetExtendedAttributeMode,
    StoreObjectHandle,
};

use crate::errors::FxfsError;
use crate::filesystem::{
    ApplyContext, FxFilesystem, JournalingObject, MAX_FILE_SIZE, TruncateGuard,
};
use crate::lsm_tree::cache::ObjectCache;
use crate::lsm_tree::types::{ItemRef, LayerIterator};
use crate::lsm_tree::{LSMTree, Query};
use crate::object_handle::{INVALID_OBJECT_ID, ObjectHandle, ObjectProperties, ReadObjectHandle};
use crate::object_store::allocator::Allocator;
use crate::object_store::base_layer::BaseLayerV61;
use crate::object_store::graveyard::Graveyard;
use crate::object_store::transaction::{
    AssocObj, AssociatedObject, LockKey, LockKeys, ObjectStoreMutation, Operation, Options,
    Transaction, lock_keys,
};
use crate::serialized_types::{
    EARLIEST_SUPPORTED_VERSION, LATEST_VERSION, Version, Versioned, VersionedLatest,
};
use anyhow::{Context, Error, anyhow, bail, ensure};
use async_trait::async_trait;
use fprint::TypeFingerprint;
use fuchsia_sync::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use storage_device::{Device, ExtentRange};
use storage_units::BlockSize;
use uuid::Uuid;

pub use extent::Extent;
pub use extent_record::{ExtentMode, ExtentValue};
pub use object_record::{
    AttributeId, AttributeKey, ExtendedAttributeValue, ObjectAttributes, ObjectKey, ObjectKeyData,
    ObjectKind, ObjectValue,
};
pub use transaction::Mutation;

/// DataObjectHandle stores an owner that must implement this trait, which allows the handle to get
/// back to an ObjectStore.
pub trait HandleOwner: AsRef<ObjectStore> + Send + Sync + 'static {}

/// StoreInfo stores information about the object store.  This is stored within the parent object
/// store, and is used, for example, to get the persistent layer objects.
pub type StoreInfo = StoreInfoV65;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, TypeFingerprint, Versioned)]
pub struct StoreInfoV65 {
    /// The globally unique identifier for the associated object store. If unset, will be all zero.
    guid: [u8; 16],

    /// The last used object ID.  Note that this field is not accurate in memory; ObjectStore's
    /// last_object_id field is the one to use in that case.  Technically, this might not be the
    /// last object ID used for the latest transaction that created an object because we use this at
    /// the point of creating the object but before we commit the transaction.  Transactions can
    /// then get committed in an arbitrary order (or not at all).
    last_object_id: u64,

    /// Object ids for layers.  TODO(https://fxbug.dev/42178036): need a layer of indirection here
    /// so we can support snapshots.
    pub layers: Vec<u64>,

    /// The object ID for the root directory.
    root_directory_object_id: u64,

    /// The object ID for the graveyard.
    graveyard_directory_object_id: u64,

    /// The number of live objects in the store.  This should *not* be trusted; it can be invalid
    /// due to filesystem inconsistencies.
    object_count: u64,

    /// A directory for storing internal files in a directory structure. Holds INVALID_OBJECT_ID
    /// when the directory doesn't yet exist.
    internal_directory_object_id: u64,

    /// cladefs: the store's layers below `layers`, newest first, which a store over a base had
    /// (shared volumes, removed, board 14527). Stored stores' information keeps decoding with them;
    /// a store that names any is refused on opening ([`ObjectStore::open`]).
    pub base_layers: Vec<BaseLayerV61>,

    /// cladefs: the cells of the extents the base layers, and the data they name, are in.
    pub base_cells: Vec<u64>,
}

impl StoreInfo {
    /// Returns the parent objects for this store.
    pub fn parent_objects(&self) -> Vec<u64> {
        // We should not include the ID of the store itself, since that should be referred to in the
        // volume directory.
        self.layers.to_vec()
    }
}

// TODO(https://fxbug.dev/42178037): We should test or put checks in place to ensure this limit isn't exceeded.
// It will likely involve placing limits on the maximum number of layers.
pub const MAX_STORE_INFO_SERIALIZED_SIZE: usize = 131072;

#[derive(Default)]
pub struct HandleOptions {
    /// If true, transactions used by this handle will skip journal space checks.
    pub skip_journal_checks: bool,
    /// If true, data written to any attribute of this handle will not have per-block checksums
    /// computed.
    pub skip_checksums: bool,
}

/// The attributes of an object a caller may set (`ObjectStore::update_attributes`), each left as
/// it is where `None`. cladefs: what Fuchsia's `fuchsia.io/MutableNodeAttributes` held of them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MutableNodeAttributes {
    /// Nanoseconds since the Unix epoch.
    pub creation_time: Option<u64>,
    /// Nanoseconds since the Unix epoch.
    pub modification_time: Option<u64>,
    /// Nanoseconds since the Unix epoch.
    pub access_time: Option<u64>,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub rdev: Option<u64>,
}

#[derive(Default)]
pub struct NewChildStoreOptions {
    /// Specifies the object ID in the root store to be used for the store.  If set to
    /// INVALID_OBJECT_ID (the default and typical case), a suitable ID will be chosen.
    pub object_id: u64,

    /// If set, use this GUID for the new store.
    pub guid: Option<[u8; 16]>,
}

/// An object store supports a file like interface for objects.  Objects are keyed by a 64 bit
/// identifier.  And object store has to be backed by a parent object store (which stores metadata
/// for the object store).  The top-level object store (a.k.a. the root parent object store) is
/// in-memory only.
pub struct ObjectStore {
    parent_store: Option<Arc<ObjectStore>>,
    store_object_id: u64,
    device: Arc<dyn Device>,
    block_size: BlockSize,
    filesystem: Weak<FxFilesystem>,
    store_info: Mutex<StoreInfo>,
    tree: LSMTree<ObjectKey, ObjectValue>,

    // When replaying the journal, the store cannot read StoreInfo until the whole journal
    // has been replayed, so during that time, store_info_handle will be None and records
    // just get sent to the tree. Once the journal has been replayed, we can open the store
    // and load all the other layer information.
    store_info_handle: OnceLock<DataObjectHandle<ObjectStore>>,

    // Set once the store's volume is deleted: it flushes no more.
    deleted: AtomicBool,

    // The number of graveyard entries, updated as they are applied.
    graveyard_entries: AtomicU64,

    // The last object ID handed out: IDs are sequential.
    last_object_id: Mutex<u64>,

    // An optional callback to be invoked each time the ObjectStore flushes.  The callback is
    // invoked at the end of flush, while the write lock is still held.
    flush_callback: Mutex<Option<Box<dyn Fn(&ObjectStore) + Send + Sync + 'static>>>,
}

impl ObjectStore {
    fn new(
        parent_store: Option<Arc<ObjectStore>>,
        store_object_id: u64,
        filesystem: Arc<FxFilesystem>,
        store_info: StoreInfo,
        object_cache: Option<Box<dyn ObjectCache<ObjectKey, ObjectValue>>>,
        last_object_id: u64,
    ) -> Arc<ObjectStore> {
        let device = filesystem.device();
        let block_size = filesystem.block_size();
        let mut store = Self::new_root_parent(device, block_size, store_object_id);
        store.parent_store = parent_store;
        store.filesystem = Arc::downgrade(&filesystem);
        *store.store_info.get_mut() = store_info;
        store.tree = LSMTree::new(merge::merge, object_cache);
        *store.last_object_id.get_mut() = last_object_id;
        Arc::from(store)
    }

    fn new_empty(
        parent_store: Option<Arc<ObjectStore>>,
        store_object_id: u64,
        filesystem: Arc<FxFilesystem>,
        object_cache: Option<Box<dyn ObjectCache<ObjectKey, ObjectValue>>>,
    ) -> Arc<Self> {
        Self::new(parent_store, store_object_id, filesystem, StoreInfo::default(), object_cache, 0)
    }

    /// Cycle breaker constructor that returns an ObjectStore without a filesystem.
    /// This should only be used from super block code.
    /// cladefs: returns `Box<Self>` so `SuperBlockManager::load` never keeps `ObjectStore` structs
    /// by value on 16 KiB guest kernel stacks (TASK-26.9).
    pub fn new_root_parent(
        device: Arc<dyn Device>,
        block_size: BlockSize,
        store_object_id: u64,
    ) -> Box<Self> {
        Box::new(ObjectStore {
            parent_store: None,
            store_object_id,
            device,
            block_size,
            filesystem: Weak::<FxFilesystem>::new(),
            store_info: Mutex::new(StoreInfo::default()),
            tree: LSMTree::new(merge::merge, None),
            store_info_handle: OnceLock::new(),
            deleted: AtomicBool::new(false),
            graveyard_entries: AtomicU64::new(0),
            last_object_id: Mutex::new(0),
            flush_callback: Mutex::new(None),
        })
    }

    /// Used to set filesystem on root_parent stores at bootstrap time after the filesystem has
    /// been created.
    /// cladefs: takes `Box<ObjectStore>` and promotes to `Arc` without moving `ObjectStore` onto
    /// the stack (TASK-26.9).
    pub fn attach_filesystem(
        mut this: Box<ObjectStore>,
        filesystem: Arc<FxFilesystem>,
    ) -> Arc<ObjectStore> {
        this.filesystem = Arc::downgrade(&filesystem);
        Arc::from(this)
    }

    /// Acquires appropriate locks and starts a new transaction.  A transaction should be associated
    /// with a store, even though it may have mutations for other objects and parent stores.
    pub async fn new_transaction<'a>(
        &self,
        locks: LockKeys,
        options: Options<'a>,
    ) -> Result<Transaction<'a>, Error> {
        let fs = self.filesystem();
        Transaction::new(fs, options, locks).await
    }

    /// Create a child store. It is a multi-step process:
    ///
    ///   1. Call `ObjectStore::new_child_store`.
    ///   2. Register the store with the object-manager.
    ///   3. Call `ObjectStore::create` to write the store-info.
    ///
    /// If the procedure fails, care must be taken to unregister store with the object-manager.
    ///
    /// The steps have to be separate because of lifetime issues when working with a transaction.
    async fn new_child_store(
        self: &Arc<Self>,
        transaction: &mut Transaction<'_>,
        options: NewChildStoreOptions,
        object_cache: Option<Box<dyn ObjectCache<ObjectKey, ObjectValue>>>,
    ) -> Result<Arc<Self>, Error> {
        let handle = if options.object_id != INVALID_OBJECT_ID {
            self.update_last_object_id(options.object_id);
            ObjectStore::create_object_with_id(
                self,
                transaction,
                options.object_id,
                HandleOptions::default(),
            )?
        } else {
            ObjectStore::create_object(self, transaction, HandleOptions::default()).await?
        };
        let store = Self::new(
            Some(self.clone()),
            handle.object_id(),
            self.filesystem(),
            StoreInfo {
                guid: options.guid.unwrap_or_else(|| *Uuid::new_v4().as_bytes()),
                ..Default::default()
            },
            object_cache,
            0,
        );
        assert!(store.store_info_handle.set(handle).is_ok());
        Ok(store)
    }

    /// Actually creates the store in a transaction.  This will also create a root directory and
    /// graveyard directory for the store.  See `new_child_store` above.
    async fn create<'a>(
        self: &'a Arc<Self>,
        transaction: &mut Transaction<'a>,
    ) -> Result<(), Error> {
        let buf = {
            // Create a root directory and graveyard directory.
            let graveyard_directory_object_id = Graveyard::create(transaction, &self).await?;
            let root_directory = Directory::create(transaction, &self).await?;

            let serialized_info = {
                let mut store_info = self.store_info.lock();

                store_info.graveyard_directory_object_id = graveyard_directory_object_id;
                store_info.root_directory_object_id = root_directory.object_id();

                let mut serialized_info = Vec::new();
                store_info.serialize_with_version(&mut serialized_info)?;
                serialized_info
            };
            let mut buf = self.device.allocate_buffer(serialized_info.len()).await;
            buf.copy_from_slice(&serialized_info[..]);
            buf
        };

        self.store_info_handle.get().unwrap().txn_write(transaction, 0u64, buf.as_ref()).await
    }

    /// Sets a callback to be invoked each time the ObjectStore flushes.  The callback is invoked at
    /// the end of flush, while the write lock is still held.
    pub fn set_flush_callback<F: Fn(&ObjectStore) + Send + Sync + 'static>(&self, callback: F) {
        let mut flush_callback = self.flush_callback.lock();
        *flush_callback = Some(Box::new(callback));
    }

    pub fn is_root(&self) -> bool {
        if let Some(parent) = &self.parent_store {
            parent.parent_store.is_none()
        } else {
            // The root parent store isn't the root store.
            false
        }
    }

    pub fn device(&self) -> &Arc<dyn Device> {
        &self.device
    }

    pub fn block_size(&self) -> BlockSize {
        self.block_size
    }

    pub fn filesystem(&self) -> Arc<FxFilesystem> {
        self.filesystem.upgrade().unwrap()
    }

    pub fn store_object_id(&self) -> u64 {
        self.store_object_id
    }

    pub fn tree(&self) -> &LSMTree<ObjectKey, ObjectValue> {
        &self.tree
    }

    pub fn root_directory_object_id(&self) -> u64 {
        self.store_info.lock().root_directory_object_id
    }

    pub fn guid(&self) -> [u8; 16] {
        self.store_info.lock().guid
    }

    pub fn graveyard_directory_object_id(&self) -> u64 {
        self.store_info.lock().graveyard_directory_object_id
    }

    fn set_graveyard_directory_object_id(&self, oid: u64) {
        assert_eq!(
            std::mem::replace(&mut self.store_info.lock().graveyard_directory_object_id, oid),
            INVALID_OBJECT_ID
        );
    }

    pub fn object_count(&self) -> u64 {
        self.store_info.lock().object_count
    }

    /// The last object ID handed out.
    pub(crate) fn last_object_id(&self) -> u64 {
        *self.last_object_id.lock()
    }

    /// Clears all in-memory caches (LSM tree object cache and persistent layer chunk caches) for
    /// this object store.
    pub fn clear_caches(&self) {
        self.tree.clear_cache();
    }

    pub fn parent_store(&self) -> Option<&Arc<ObjectStore>> {
        self.parent_store.as_ref()
    }

    /// Returns the file size for the object without opening the object.
    #[cfg(test)]
    async fn get_file_size(&self, object_id: u64) -> Result<u64, Error> {
        self.tree
            .find_map(
                &ObjectKey::attribute(object_id, AttributeId::DATA, AttributeKey::Attribute),
                |item| match item.value {
                    ObjectValue::Attribute { size, .. } => Ok(*size),
                    _ => Err(anyhow!(FxfsError::NotFile)),
                },
            )
            .await?
            .ok_or(FxfsError::NotFound)?
    }

    /// Opens the object `obj_id`'s data.  Users should avoid having more than one handle open for
    /// the same object at the same time because they might get out-of-sync; there is no code that
    /// will prevent this.
    pub async fn open_object<S: HandleOwner>(
        owner: &Arc<S>,
        obj_id: u64,
        options: HandleOptions,
    ) -> Result<DataObjectHandle<S>, Error> {
        let store = owner.as_ref().as_ref();
        let value = store
            .tree
            .find_value(&ObjectKey::attribute(obj_id, AttributeId::DATA, AttributeKey::Attribute))
            .await?
            .ok_or(FxfsError::NotFound)?;

        let ObjectValue::Attribute { size } = value else {
            bail!(anyhow!(FxfsError::Inconsistent).context("open_object: Expected attibute"));
        };

        ensure!(size <= MAX_FILE_SIZE, FxfsError::Inconsistent);

        Ok(DataObjectHandle::new(owner.clone(), obj_id, AttributeId::DATA, size, options))
    }

    pub fn create_object_with_id<S: HandleOwner>(
        owner: &Arc<S>,
        transaction: &mut Transaction<'_>,
        object_id: u64,
        options: HandleOptions,
    ) -> Result<DataObjectHandle<S>, Error> {
        let store = owner.as_ref().as_ref();
        let now = Timestamp::now();
        assert!(
            transaction
                .add(
                    store.store_object_id(),
                    Mutation::insert_object(
                        ObjectKey::object(object_id),
                        ObjectValue::file(1, 0, now.clone(), now.clone(), now.clone(), now, None,),
                    ),
                )
                .is_none()
        );
        transaction.add(
            store.store_object_id(),
            Mutation::insert_object(
                ObjectKey::attribute(object_id, AttributeId::DATA, AttributeKey::Attribute),
                ObjectValue::attribute(0),
            ),
        );
        Ok(DataObjectHandle::new(owner.clone(), object_id, AttributeId::DATA, 0, options))
    }

    /// Creates an object in the store.
    pub async fn create_object<S: HandleOwner>(
        owner: &Arc<S>,
        mut transaction: &mut Transaction<'_>,
        options: HandleOptions,
    ) -> Result<DataObjectHandle<S>, Error> {
        let store = owner.as_ref().as_ref();
        let object_id = store.get_next_object_id()?;
        ObjectStore::create_object_with_id(owner, &mut transaction, object_id, options)
    }

    /// Adjusts the reference count for a given object.  If the reference count reaches zero, the
    /// object is moved into the graveyard and true is returned.
    pub async fn adjust_refs(
        &self,
        transaction: &mut Transaction<'_>,
        object_id: u64,
        delta: i64,
    ) -> Result<bool, Error> {
        self.adjust_refs_impl(transaction, object_id, delta, true).await
    }

    /// Adjusts the reference count for a given object.  If the reference count reaches zero and
    /// `add_to_graveyard` is true, the object is moved into the graveyard.  Returns whether the
    /// reference count reached zero.
    pub async fn adjust_refs_impl(
        &self,
        transaction: &mut Transaction<'_>,
        object_id: u64,
        delta: i64,
        add_to_graveyard: bool,
    ) -> Result<bool, Error> {
        let mut mutation = self.txn_get_object_mutation(transaction, object_id).await?;
        let refs = if let ObjectValue::Object {
            kind: ObjectKind::File { refs, .. } | ObjectKind::Symlink { refs, .. },
            ..
        } = &mut mutation.item.value
        {
            *refs =
                refs.checked_add_signed(delta).ok_or_else(|| anyhow!("refs underflow/overflow"))?;
            refs
        } else {
            bail!(FxfsError::NotFile);
        };
        if *refs == 0 {
            if add_to_graveyard {
                self.add_to_graveyard(transaction, object_id);

                // We might still need to adjust the reference count if delta was something other
                // than -1.
                if delta != -1 {
                    *refs = 1;
                    transaction.add(self.store_object_id, Mutation::ObjectStore(mutation));
                }

                // Otherwise, we don't commit the mutation as we want to keep reference count as 1
                // for objects in graveyard.
            }
            Ok(true)
        } else {
            transaction.add(self.store_object_id, Mutation::ObjectStore(mutation));
            Ok(false)
        }
    }

    /// Purges an object that is in the graveyard. If a truncate guard is not provided, one will be
    /// acquired.
    pub async fn tombstone_object(
        &self,
        object_id: u64,
        txn_options: Options<'_>,
        truncate_guard: Option<&TruncateGuard<'_>>,
    ) -> Result<(), Error> {
        // cladefs: errors, where upstream asserts: what's in a graveyard, reaped from a mount on, is
        // what an image says, of an object that may not be there.
        ensure!(
            self.tree.exists(&ObjectKey::object(object_id)).await?,
            anyhow!(FxfsError::Inconsistent).context("Tombstoning missing object")
        );
        ensure!(
            self.tree
                .exists(&ObjectKey::graveyard_entry(
                    self.graveyard_directory_object_id(),
                    object_id
                ))
                .await?,
            anyhow!(FxfsError::Inconsistent).context("Tombstoning object not in graveyard")
        );
        let _fs;
        let _guard;
        let truncate_guard = match truncate_guard {
            Some(guard) => guard,
            None => {
                _fs = self.filesystem();
                _guard = _fs.truncate_guard(self.store_object_id, object_id).await;
                &_guard
            }
        };
        self.trim_or_tombstone(object_id, true, txn_options, truncate_guard).await
    }

    /// cladefs: tombstones the objects `object_ids`, which are in the graveyard, as `tombstone_object`
    /// does each, but as many to a transaction as fit (`txn_tombstone_unlinked`): a small file's
    /// tombstone is a few mutations, which a transaction of its own costs several times over. An
    /// object with more than one attribute, or more extents than fit, is finished on its own.
    pub async fn tombstone_objects(
        &self,
        object_ids: &[u64],
        txn_options: Options<'_>,
    ) -> Result<(), Error> {
        // The most objects a transaction tombstones: the locks it takes, whatever room it has.
        const BATCH_OBJECTS: usize = 32;
        let fs = self.filesystem();
        let store_id = self.store_object_id;
        let mut rest = object_ids;
        while !rest.is_empty() {
            let batch = &rest[..rest.len().min(BATCH_OBJECTS)];
            let mut truncate_keys = LockKeys::with_capacity(batch.len());
            let mut keys = LockKeys::with_capacity(2 * batch.len());
            for &object_id in batch {
                truncate_keys.push(LockKey::truncate(store_id, object_id));
                keys.push(LockKey::object_attribute(
                    store_id,
                    object_id,
                    AttributeId::SORTED_START,
                ));
                keys.push(LockKey::object(store_id, object_id));
            }
            let truncate_guard = fs.lock_manager().write_lock(truncate_keys).await;
            let mut transaction = self.new_transaction(keys, txn_options).await?;
            let mut unfinished = Vec::new();
            let mut done = 0;
            for &object_id in batch {
                done += 1;
                if !self.txn_tombstone_unlinked(&mut transaction, object_id).await? {
                    unfinished.push(object_id);
                }
                // What a full transaction leaves goes in the next.
                if transaction.is_full() {
                    break;
                }
            }
            if transaction.mutations().is_empty() {
                std::mem::drop(transaction);
            } else {
                transaction.commit().await?;
            }
            // `tombstone_object` takes the locks it needs itself.
            std::mem::drop(truncate_guard);
            for object_id in unfinished {
                self.tombstone_object(object_id, txn_options, None).await?;
            }
            rest = &rest[done..];
        }
        Ok(())
    }

    /// Trim extents beyond the end of a file for all attributes.  This will remove the entry from
    /// the graveyard when done.
    pub async fn trim(
        &self,
        object_id: u64,
        truncate_guard: &TruncateGuard<'_>,
    ) -> Result<(), Error> {
        // For the root and root parent store, we would need to use the metadata reservation which
        // we don't currently support, so assert that we're not those stores.
        assert!(self.parent_store.as_ref().unwrap().parent_store.is_some());

        self.trim_or_tombstone(
            object_id,
            false,
            Options { borrow_metadata_space: true, ..Default::default() },
            truncate_guard,
        )
        .await
    }

    /// Trims or tombstones an object.
    async fn trim_or_tombstone(
        &self,
        object_id: u64,
        for_tombstone: bool,
        txn_options: Options<'_>,
        _truncate_guard: &TruncateGuard<'_>,
    ) -> Result<(), Error> {
        let mut next_attribute = Some(AttributeId::SORTED_START);
        while let Some(attribute_id) = next_attribute.take() {
            let mut transaction = self
                .new_transaction(
                    lock_keys![
                        LockKey::object_attribute(self.store_object_id, object_id, attribute_id),
                        LockKey::object(self.store_object_id, object_id),
                    ],
                    txn_options,
                )
                .await?;

            match self
                .trim_some(
                    &mut transaction,
                    object_id,
                    attribute_id,
                    if for_tombstone {
                        TrimMode::Tombstone(TombstoneMode::Object)
                    } else {
                        TrimMode::UseSize
                    },
                )
                .await?
            {
                TrimResult::Incomplete => next_attribute = Some(attribute_id),
                TrimResult::Done(None) => {
                    if for_tombstone
                        || matches!(
                            self.tree
                                .find_value(&ObjectKey::graveyard_entry(
                                    self.graveyard_directory_object_id(),
                                    object_id,
                                ))
                                .await?,
                            Some(ObjectValue::Trim)
                        )
                    {
                        self.remove_from_graveyard(&mut transaction, object_id);
                    }
                    // The last attribute was not the default attribute, it may have been added to
                    // the graveyard alongside the object.
                    if for_tombstone && attribute_id != AttributeId::DATA {
                        self.remove_attribute_from_graveyard(
                            &mut transaction,
                            object_id,
                            attribute_id,
                        );
                    }
                }
                TrimResult::Done(id) => {
                    // Moved to the next attribute. This one is finished and it may have been
                    // added to the graveyard alongside the object.
                    if for_tombstone && attribute_id != AttributeId::DATA {
                        self.remove_attribute_from_graveyard(
                            &mut transaction,
                            object_id,
                            attribute_id,
                        );
                    }
                    next_attribute = id;
                }
            }

            if !transaction.mutations().is_empty() {
                transaction.commit().await?;
            }
        }
        Ok(())
    }

    // Purges an object's attribute that is in the graveyard.
    pub async fn tombstone_attribute(
        &self,
        object_id: u64,
        attribute_id: AttributeId,
        txn_options: Options<'_>,
    ) -> Result<(), Error> {
        // Ensure that we don't double-delete things, it should still exist and be in the graveyard.
        debug_assert!(
            self.tree
                .exists(&ObjectKey::attribute(object_id, attribute_id, AttributeKey::Attribute))
                .await?,
            "Tombstoning missing attribute"
        );
        debug_assert!(
            self.tree
                .exists(&ObjectKey::graveyard_attribute_entry(
                    self.graveyard_directory_object_id(),
                    object_id,
                    attribute_id
                ))
                .await?,
            "Tombstoning attribute not in graveyard"
        );
        let mut trim_result = TrimResult::Incomplete;
        while matches!(trim_result, TrimResult::Incomplete) {
            let mut transaction = self
                .new_transaction(
                    lock_keys![
                        LockKey::object_attribute(self.store_object_id, object_id, attribute_id),
                        LockKey::object(self.store_object_id, object_id),
                    ],
                    txn_options,
                )
                .await?;
            trim_result = self
                .trim_some(
                    &mut transaction,
                    object_id,
                    attribute_id,
                    TrimMode::Tombstone(TombstoneMode::Attribute),
                )
                .await?;
            if let TrimResult::Done(..) = trim_result {
                self.remove_attribute_from_graveyard(&mut transaction, object_id, attribute_id)
            }
            if !transaction.mutations().is_empty() {
                transaction.commit().await?;
            }
        }
        Ok(())
    }

    /// Deletes extents for attribute `attribute_id` in object `object_id`.  Also see the comments
    /// for TrimMode and TrimResult. Should hold a lock on the attribute, and the object as it
    /// performs a read-modify-write on the sizes.
    pub async fn trim_some(
        &self,
        transaction: &mut Transaction<'_>,
        object_id: u64,
        attribute_id: AttributeId,
        mode: TrimMode,
    ) -> Result<TrimResult, Error> {
        let layer_set = self.tree.layer_set();
        let mut merger = layer_set.merger();

        let aligned_offset = match mode {
            TrimMode::FromOffset(offset) => {
                self.block_size.align_up(offset).ok_or(FxfsError::Inconsistent)?
            }
            TrimMode::Tombstone(..) => 0,
            TrimMode::UseSize => {
                let iter = merger
                    .query(Query::FullRange(&ObjectKey::attribute(
                        object_id,
                        attribute_id,
                        AttributeKey::Attribute,
                    )))
                    .await?;
                if let Some(item_ref) = iter.get() {
                    if item_ref.key.object_id != object_id {
                        return Ok(TrimResult::Done(None));
                    }

                    if let ItemRef {
                        key:
                            ObjectKey {
                                data:
                                    ObjectKeyData::Attribute(size_attribute_id, AttributeKey::Attribute),
                                ..
                            },
                        value: ObjectValue::Attribute { size, .. },
                        ..
                    } = item_ref
                    {
                        // If we found a different attribute_id, return so we can get the
                        // right lock.
                        if *size_attribute_id != attribute_id {
                            return Ok(TrimResult::Done(Some(*size_attribute_id)));
                        }
                        self.block_size.align_up(*size).ok_or(FxfsError::Inconsistent)?
                    } else if let ObjectKeyData::Attribute(found, _) = &item_ref.key.data {
                        // Asserting that this is a size record would be brittle, so just skip to
                        // the next attribute instead: a removed one's tombstone is not.
                        return Ok(TrimResult::Done(Some(found.next())));
                    } else {
                        // cladefs: past the attributes, there are none to trim: an object's extended
                        // attributes sort after them, and this would find them from each attribute
                        // ID in turn, of 2^64, the graveyard's task spinning all the while.
                        return Ok(TrimResult::Done(None));
                    }
                } else {
                    // End of the tree.
                    return Ok(TrimResult::Done(None));
                }
            }
        };

        // Loop over the extents and deallocate them.
        let mut iter = merger
            .query(Query::FullRange(&ObjectKey::from_extent(
                object_id,
                attribute_id,
                Extent::search_key_from_offset(aligned_offset),
            )))
            .await?;
        let mut end = 0;
        let allocator = self.allocator();
        let mut result = TrimResult::Done(None);
        let mut deallocated = 0;
        let block_size = self.block_size;

        while let Some(item_ref) = iter.get() {
            if item_ref.key.object_id != object_id {
                break;
            }
            if let ObjectKey {
                data: ObjectKeyData::Attribute(extent_attribute_id, attribute_key),
                ..
            } = item_ref.key
            {
                if *extent_attribute_id != attribute_id {
                    result = TrimResult::Done(Some(*extent_attribute_id));
                    break;
                }
                if let (
                    AttributeKey::Extent(extent),
                    ObjectValue::Extent(ExtentValue::Some {
                        extent: store_extent,
                        extent_offset,
                        ..
                    }),
                ) = (attribute_key, item_ref.value)
                {
                    let start = std::cmp::max(extent.start, aligned_offset);
                    ensure!(start < extent.end, FxfsError::Inconsistent);
                    let device_offset = extent_offset
                        .checked_add(start - extent.start)
                        .ok_or(FxfsError::Inconsistent)?;
                    end = extent.end;
                    let len = end - start;
                    let device_range =
                        ExtentRange::new(*store_extent, device_offset..device_offset + len);
                    ensure!(block_size.is_aligned(&device_range.range), FxfsError::Inconsistent);
                    allocator.deallocate(transaction, self.store_object_id, device_range).await?;
                    deallocated += len;
                    // Stop if the transaction is getting too big. cladefs: as it says, in what it
                    // takes of the journal.
                    if transaction.is_full() {
                        result = TrimResult::Incomplete;
                        break;
                    }
                }
            }
            iter.advance().await?;
        }

        let finished_tombstone_object = matches!(mode, TrimMode::Tombstone(TombstoneMode::Object))
            && matches!(result, TrimResult::Done(None));
        let finished_tombstone_attribute =
            matches!(mode, TrimMode::Tombstone(TombstoneMode::Attribute))
                && !matches!(result, TrimResult::Incomplete);

        // Deletion marker records *must* be merged so as to consume all other records for the
        // object.
        if finished_tombstone_object {
            // cladefs: not a panic: the graveyard's entry is what an image says.
            let mutation = self.txn_get_object_mutation(transaction, object_id).await?;
            ensure!(
                matches!(mutation.item.value, ObjectValue::Object { .. }),
                anyhow!(FxfsError::Inconsistent).context("Inconsistent object type")
            );
            transaction.add(
                self.store_object_id,
                Mutation::merge_object(ObjectKey::object(object_id), ObjectValue::None),
            );
        } else {
            if finished_tombstone_attribute {
                transaction.add(
                    self.store_object_id,
                    Mutation::merge_object(
                        ObjectKey::attribute(object_id, attribute_id, AttributeKey::Attribute),
                        ObjectValue::None,
                    ),
                );
            }
            if deallocated > 0 {
                let mut mutation = self.txn_get_object_mutation(transaction, object_id).await?;
                transaction.add(
                    self.store_object_id,
                    Mutation::merge_object(
                        ObjectKey::extent(object_id, attribute_id, aligned_offset..end),
                        ObjectValue::deleted_extent(),
                    ),
                );
                // Update allocated size.
                if let ObjectValue::Object {
                    attributes: ObjectAttributes { allocated_size, .. },
                    ..
                } = &mut mutation.item.value
                {
                    // The only way for these to fail are if the volume is inconsistent.
                    *allocated_size = allocated_size.checked_sub(deallocated).ok_or_else(|| {
                        anyhow!(FxfsError::Inconsistent).context("Allocated size overflow")
                    })?;
                } else {
                    // cladefs: not a panic, as above.
                    bail!(anyhow!(FxfsError::Inconsistent).context("Unexpected object value"));
                }
                transaction.add(self.store_object_id, Mutation::ObjectStore(mutation));
            }
        }
        Ok(result)
    }

    /// cladefs: tombstones `object_id`, which has no names left, in `transaction`, as the graveyard's
    /// `tombstone_object` would in transactions of its own: and returns whether it did. What
    /// tombstones an unlinked object in a transaction of its caller's does so here: the graveyard's
    /// batches (`tombstone_objects`), an unlink that frees what it unlinks (fxfs-posix's queued
    /// unlinks, `replace_child_with_purge`). The caller holds the object's lock, its attributes'
    /// (AttributeId::SORTED_START) and its truncate lock.
    ///
    /// The object is in the graveyard, or this fails, as `tombstone_object` does: its entry is the
    /// transaction's own, which the unlink of its last name adds (`adjust_refs`), or the tree's;
    /// or both, the tree's a truncate's trim, which ran out of room before it removed it. A
    /// tombstone takes the entry with it, whichever it is: the transaction's is taken back, the
    /// tree's removed (TASK-82: fsck found a trim's entry of an object gone).
    ///
    /// It doesn't tombstone, and leaves the object in the graveyard, whatever it frees of it, if
    /// the transaction is full or fills first, the object having more extents than it has room
    /// for, or if the object has attributes other than its data. An object with nothing allocated,
    /// as files never written are, has no extents to walk.
    pub async fn txn_tombstone_unlinked(
        &self,
        transaction: &mut Transaction<'_>,
        object_id: u64,
    ) -> Result<bool, Error> {
        if transaction.is_full() {
            return Ok(false);
        }
        let entry = ObjectKey::graveyard_entry(self.graveyard_directory_object_id(), object_id);
        let in_tree = self.tree.find(&entry).await?.is_some();
        let in_graveyard =
            match transaction.get_object_mutation(self.store_object_id, entry.clone()) {
                Some(mutation) => mutation.item.value != ObjectValue::None,
                None => in_tree,
            };
        ensure!(
            in_graveyard,
            anyhow!(FxfsError::Inconsistent).context("Tombstoning object not in graveyard")
        );
        let tombstoned = self.tombstone_if_unallocated(transaction, object_id).await?
            || matches!(
                self.trim_some(
                    transaction,
                    object_id,
                    AttributeId::SORTED_START,
                    TrimMode::Tombstone(TombstoneMode::Object),
                )
                .await?,
                TrimResult::Done(None)
            );
        if tombstoned {
            if in_tree {
                self.remove_from_graveyard(transaction, object_id);
            } else {
                transaction.remove(
                    self.store_object_id,
                    Mutation::replace_or_insert_object(entry, ObjectValue::None),
                );
            }
        }
        Ok(tombstoned)
    }

    /// cladefs: tombstones `object_id` in `transaction`, if it has nothing allocated, as `trim_some`
    /// (TrimMode::Tombstone(TombstoneMode::Object)) would, without walking its extents, of which
    /// it has none: and returns whether it did. An object with storage allocated is left to
    /// `trim_some`. Nor has it attributes in the graveyard, which their writes' extents put there.
    async fn tombstone_if_unallocated(
        &self,
        transaction: &mut Transaction<'_>,
        object_id: u64,
    ) -> Result<bool, Error> {
        let mutation = self.txn_get_object_mutation(transaction, object_id).await?;
        let ObjectValue::Object { attributes: ObjectAttributes { allocated_size, .. }, .. } =
            mutation.item.value
        else {
            bail!(anyhow!(FxfsError::Inconsistent).context("tombstone: expected an object"));
        };
        if allocated_size != 0 {
            return Ok(false);
        }
        // Deletion marker records *must* be merged so as to consume all other records for the
        // object.
        transaction.add(
            self.store_object_id,
            Mutation::merge_object(ObjectKey::object(object_id), ObjectValue::None),
        );
        Ok(true)
    }

    /// Returns all objects that exist in the parent store that pertain to this object store.
    /// Note that this doesn't include the object_id of the store itself which is generally
    /// referenced externally.
    pub fn parent_objects(&self) -> Vec<u64> {
        assert!(self.store_info_handle.get().is_some());
        self.store_info.lock().parent_objects()
    }

    /// Returns root objects for this store.
    pub fn root_objects(&self) -> Vec<u64> {
        let mut objects = Vec::new();
        let info = self.store_info.lock();
        if info.root_directory_object_id != INVALID_OBJECT_ID {
            objects.push(info.root_directory_object_id);
        }
        if info.graveyard_directory_object_id != INVALID_OBJECT_ID {
            objects.push(info.graveyard_directory_object_id);
        }
        if info.internal_directory_object_id != INVALID_OBJECT_ID {
            objects.push(info.internal_directory_object_id);
        }
        objects
    }

    pub fn store_info(&self) -> StoreInfo {
        self.store_info.lock().clone()
    }

    /// Returns None if called during journal replay.
    pub fn store_info_handle_object_id(&self) -> Option<u64> {
        self.store_info_handle.get().map(|h| h.object_id())
    }

    pub fn graveyard_count(&self) -> u64 {
        self.graveyard_entries.load(Ordering::Relaxed)
    }

    /// Called to open a store, before replay of this store's mutations.
    async fn open(
        parent_store: &Arc<ObjectStore>,
        store_object_id: u64,
        object_cache: Option<Box<dyn ObjectCache<ObjectKey, ObjectValue>>>,
    ) -> Result<Arc<ObjectStore>, Error> {
        let handle =
            ObjectStore::open_object(parent_store, store_object_id, HandleOptions::default())
                .await?;

        let info = load_store_info(parent_store, store_object_id).await?;
        // cladefs: what adds to a graveyard asserts there is one, as there is of any store that was
        // made: this is what an image says.
        ensure!(
            info.graveyard_directory_object_id != INVALID_OBJECT_ID,
            anyhow!(FxfsError::Inconsistent).context("A store with no graveyard")
        );

        // cladefs: a store over a base, once made by shared volumes (removed, board 14527), has
        // no bottom layers any more: opened as a plain volume, it would show a tree without them.
        ensure!(
            info.base_layers.is_empty(),
            anyhow!(FxfsError::NotSupported).context(format!(
                "Object store {store_object_id} has layers of a base: the filesystem's shared \
                 volumes were removed, and a volume over one is not opened"
            ))
        );

        // TODO(https://fxbug.dev/42178043): the layer size here could be bad and cause overflow.

        let last_object_id = info.last_object_id;
        let object_tree_layer_object_ids = info.layers.clone();

        let fs = parent_store.filesystem();

        let store = ObjectStore::new(
            Some(parent_store.clone()),
            store_object_id,
            fs.clone(),
            info,
            object_cache,
            last_object_id,
        );

        assert!(store.store_info_handle.set(handle).is_ok(), "Failed to set store_info_handle!");

        let total_layer_size = store
            .tree
            .append_layers(&parent_store, object_tree_layer_object_ids)
            .await
            .context("Failed to read object store layers")?;
        fs.object_manager().update_reservation(
            store_object_id,
            tree::reservation_amount_from_layer_size(total_layer_size),
        );

        Ok(store)
    }

    #[cfg(test)]
    async fn load_store_info(&self) -> Result<StoreInfo, Error> {
        load_store_info_from_handle(self.store_info_handle.get().unwrap()).await
    }

    /// Returns a new object ID that can be used.
    pub(super) fn get_next_object_id(&self) -> Result<u64, Error> {
        let mut last_object_id = self.last_object_id.lock();
        // IDs that wrapped, which within a store's lifetime means corruption.
        let id = last_object_id.checked_add(1).ok_or(FxfsError::Inconsistent)?;
        *last_object_id = id;
        Ok(id)
    }

    /// Query the next object ID that will be used. Intended for use when checking filesystem
    /// consistency. Prefer [`Self::get_next_object_id()`] for general use.
    pub(crate) fn query_next_object_id(&self) -> u64 {
        self.last_object_id.lock().wrapping_add(1)
    }

    fn allocator(&self) -> Arc<Allocator> {
        self.filesystem().allocator()
    }

    // If |transaction| has an impending mutation for the underlying object, returns that.
    // Otherwise, looks up the object from the tree and returns a suitable mutation for it.  The
    // mutation is returned here rather than the item because the mutation includes the operation
    // which has significance: inserting an object implies it's the first of its kind unlike
    // replacing an object.
    async fn txn_get_object_mutation(
        &self,
        transaction: &Transaction<'_>,
        object_id: u64,
    ) -> Result<ObjectStoreMutation, Error> {
        if let Some(mutation) =
            transaction.get_object_mutation(self.store_object_id, ObjectKey::object(object_id))
        {
            Ok(mutation.clone())
        } else {
            Ok(ObjectStoreMutation {
                item: self
                    .tree
                    .find(&ObjectKey::object(object_id))
                    .await?
                    .ok_or(FxfsError::Inconsistent)
                    .context("Object id missing")?,
                op: Operation::ReplaceOrInsert,
            })
        }
    }

    fn update_last_object_id(&self, object_id: u64) {
        let mut id = self.last_object_id.lock();
        *id = (*id).max(object_id);
    }

    /// Adds the specified object to the graveyard.
    pub fn add_to_graveyard(&self, transaction: &mut Transaction<'_>, object_id: u64) {
        let graveyard_id = self.graveyard_directory_object_id();
        assert_ne!(graveyard_id, INVALID_OBJECT_ID);
        transaction.add(
            self.store_object_id,
            Mutation::replace_or_insert_object(
                ObjectKey::graveyard_entry(graveyard_id, object_id),
                ObjectValue::Some,
            ),
        );
    }

    /// Removes the specified object from the graveyard.  NB: Care should be taken when calling
    /// this because graveyard entries are used for purging deleted files *and* for trimming
    /// extents.  For example, consider the following sequence:
    ///
    ///     1. Add Trim graveyard entry.
    ///     2. Replace with Some graveyard entry (see above).
    ///     3. Remove graveyard entry.
    ///
    /// If the desire in #3 is just to cancel the effect of the Some entry, then #3 should
    /// actually be:
    ///
    ///     3. Replace with Trim graveyard entry.
    pub fn remove_from_graveyard(&self, transaction: &mut Transaction<'_>, object_id: u64) {
        transaction.add(
            self.store_object_id,
            Mutation::replace_or_insert_object(
                ObjectKey::graveyard_entry(self.graveyard_directory_object_id(), object_id),
                ObjectValue::None,
            ),
        );
    }

    /// Removes the specified attribute from the graveyard. Unlike object graveyard entries,
    /// attribute graveyard entries only have one functionality (i.e. to purge deleted attributes)
    /// so the caller does not need to be concerned about replacing the graveyard attribute entry
    /// with its prior state when cancelling it. See comment on `remove_from_graveyard()`.
    pub fn remove_attribute_from_graveyard(
        &self,
        transaction: &mut Transaction<'_>,
        object_id: u64,
        attribute_id: AttributeId,
    ) {
        transaction.add(
            self.store_object_id,
            Mutation::replace_or_insert_object(
                ObjectKey::graveyard_attribute_entry(
                    self.graveyard_directory_object_id(),
                    object_id,
                    attribute_id,
                ),
                ObjectValue::None,
            ),
        );
    }

    /// Returns the link of a symlink object.
    pub async fn read_symlink(&self, object_id: u64) -> Result<Vec<u8>, Error> {
        match self.tree.find_value(&ObjectKey::object(object_id)).await? {
            None => bail!(FxfsError::NotFound),
            Some(ObjectValue::Object { kind: ObjectKind::Symlink { link, .. }, .. }) => {
                if !link.is_empty() {
                    return Ok(link.into_vec());
                }
                // cladefs: a long target, in its chunks (see `directory::LONG_SYMLINK_XATTR`).
                let mut target = Vec::new();
                for index in 0.. {
                    let key = ObjectKey::extended_attribute(
                        object_id,
                        directory::long_symlink_xattr(index),
                    );
                    match self.tree.find_value(&key).await? {
                        Some(ObjectValue::ExtendedAttribute(ExtendedAttributeValue::Inline(
                            chunk,
                        ))) => target.extend_from_slice(&chunk),
                        None => break,
                        Some(value) => bail!(
                            anyhow!(FxfsError::Inconsistent)
                                .context(format!("Unexpected value in a long symlink: {value:?}"))
                        ),
                    }
                }
                ensure!(!target.is_empty(), FxfsError::Inconsistent);
                Ok(target)
            }
            Some(value) => Err(anyhow!(FxfsError::Inconsistent)
                .context(format!("Unexpected value in symlink lookup: {value:?}"))),
        }
    }

    pub async fn update_attributes<'a>(
        &self,
        transaction: &mut Transaction<'a>,
        object_id: u64,
        node_attributes: Option<&MutableNodeAttributes>,
        change_time: Option<Timestamp>,
    ) -> Result<(), Error> {
        if change_time.is_none()
            && node_attributes.is_none_or(|attributes| *attributes == Default::default())
        {
            return Ok(());
        }
        let mut mutation = self.txn_get_object_mutation(transaction, object_id).await?;
        if let ObjectValue::Object { ref mut attributes, .. } = mutation.item.value {
            if let Some(time) = change_time {
                attributes.change_time = time;
            }
            if let Some(node_attributes) = node_attributes {
                if let Some(time) = node_attributes.creation_time {
                    attributes.creation_time = Timestamp::from_nanos(time);
                }
                if let Some(time) = node_attributes.modification_time {
                    attributes.modification_time = Timestamp::from_nanos(time);
                }
                if let Some(time) = node_attributes.access_time {
                    attributes.access_time = Timestamp::from_nanos(time);
                }
                if node_attributes.mode.is_some()
                    || node_attributes.uid.is_some()
                    || node_attributes.gid.is_some()
                    || node_attributes.rdev.is_some()
                {
                    if let Some(a) = &mut attributes.posix_attributes {
                        if let Some(mode) = node_attributes.mode {
                            a.mode = mode;
                        }
                        if let Some(uid) = node_attributes.uid {
                            a.uid = uid;
                        }
                        if let Some(gid) = node_attributes.gid {
                            a.gid = gid;
                        }
                        if let Some(rdev) = node_attributes.rdev {
                            a.rdev = rdev;
                        }
                    } else {
                        attributes.posix_attributes = Some(PosixAttributes {
                            mode: node_attributes.mode.unwrap_or_default(),
                            uid: node_attributes.uid.unwrap_or_default(),
                            gid: node_attributes.gid.unwrap_or_default(),
                            rdev: node_attributes.rdev.unwrap_or_default(),
                        });
                    }
                }
            }
        } else {
            bail!(
                anyhow!(FxfsError::Inconsistent)
                    .context("ObjectStore.update_attributes: Expected object value")
            );
        };
        transaction.add(self.store_object_id(), Mutation::ObjectStore(mutation));
        Ok(())
    }

    // Updates and commits the changes to access time in ObjectProperties. The update matches
    // Linux's RELATIME. That is, access time is updated to the current time if access time is less
    // than or equal to the last modification or status change, or if it has been more than a day
    // since the last access.  `precondition` is a condition to be checked *after* taking the lock
    // on the object.  If `precondition` returns false, no update will be performed.
    pub async fn update_access_time(
        &self,
        object_id: u64,
        props: &mut ObjectProperties,
        precondition: impl FnOnce() -> bool,
    ) -> Result<(), Error> {
        let access_time = props.access_time.as_nanos();
        let modification_time = props.modification_time.as_nanos();
        let change_time = props.change_time.as_nanos();
        let now = Timestamp::now();
        if access_time <= modification_time
            || access_time <= change_time
            || access_time
                < now.as_nanos()
                    - Timestamp::from(std::time::Duration::from_secs(24 * 60 * 60)).as_nanos()
        {
            let mut transaction = self
                .new_transaction(
                    lock_keys![LockKey::object(self.store_object_id, object_id,)],
                    Options { borrow_metadata_space: true, ..Default::default() },
                )
                .await?;
            if precondition() {
                self.update_attributes(
                    &mut transaction,
                    object_id,
                    Some(&MutableNodeAttributes {
                        access_time: Some(now.as_nanos()),
                        ..Default::default()
                    }),
                    None,
                )
                .await?;
                transaction.commit().await?;
                props.access_time = now;
            }
        }
        Ok(())
    }

    async fn write_store_info<'a>(
        &'a self,
        transaction: &mut Transaction<'a>,
        info: &StoreInfo,
    ) -> Result<(), Error> {
        let mut serialized_info = Vec::new();
        info.serialize_with_version(&mut serialized_info)?;
        let mut buf = self.device.allocate_buffer(serialized_info.len()).await;
        buf.copy_from_slice(&serialized_info);
        self.store_info_handle.get().unwrap().txn_write(transaction, 0u64, buf.as_ref()).await
    }

    pub fn mark_deleted(&self) {
        self.deleted.store(true, Ordering::Relaxed);
    }

    /// Whether the store's volume was deleted (`mark_deleted`).
    pub(crate) fn is_deleted(&self) -> bool {
        self.deleted.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn test_set_last_object_id(&self, object_id: u64) {
        *self.last_object_id.lock() = object_id;
    }

    /// Looks up the size of the attribute. Returns an error if either the object or attribute
    /// doesn't exist.
    pub async fn get_attribute_size(
        &self,
        object_id: u64,
        attribute_id: AttributeId,
    ) -> Result<u64, Error> {
        let size = self
            .tree
            .find_map(
                &ObjectKey::attribute(object_id, attribute_id, AttributeKey::Attribute),
                |item| match item.value {
                    ObjectValue::Attribute { size, .. } => Ok(*size),
                    _ => Err(anyhow!(FxfsError::Inconsistent)),
                },
            )
            .await?
            .ok_or(FxfsError::NotFound)??;
        Ok(size)
    }
}

#[async_trait]
impl JournalingObject for ObjectStore {
    fn apply_mutation(
        &self,
        mutation: Mutation,
        context: &ApplyContext<'_, '_>,
        _assoc_obj: AssocObj<'_>,
    ) -> Result<(), Error> {
        match mutation {
            Mutation::ObjectStore(ObjectStoreMutation { item, op }) => {
                match op {
                    Operation::Insert => {
                        // If we are inserting an object record for the first time, it signifies the
                        // birth of the object so we need to adjust the object count.
                        if matches!(item.value, ObjectValue::Object { .. }) {
                            {
                                let info = &mut self.store_info.lock();
                                let object_count = &mut info.object_count;
                                *object_count = object_count.saturating_add(1);
                            }
                            if context.mode.is_replay() {
                                self.update_last_object_id(item.key.object_id);
                            }
                        } else if !context.mode.is_replay()
                            && matches!(
                                item.key.data,
                                ObjectKeyData::GraveyardEntry { .. }
                                    | ObjectKeyData::GraveyardAttributeEntry { .. }
                            )
                        {
                            if matches!(item.value, ObjectValue::Some | ObjectValue::Trim) {
                                self.graveyard_entries.fetch_add(1, Ordering::Relaxed);
                            } else if matches!(item.value, ObjectValue::None) {
                                self.graveyard_entries.fetch_sub(1, Ordering::Relaxed);
                            }
                        }
                        self.tree.insert(item)?;
                    }
                    Operation::ReplaceOrInsert => {
                        if !context.mode.is_replay()
                            && matches!(
                                item.key.data,
                                ObjectKeyData::GraveyardEntry { .. }
                                    | ObjectKeyData::GraveyardAttributeEntry { .. }
                            )
                        {
                            if matches!(item.value, ObjectValue::Some | ObjectValue::Trim) {
                                self.graveyard_entries.fetch_add(1, Ordering::Relaxed);
                            } else if matches!(item.value, ObjectValue::None) {
                                self.graveyard_entries.fetch_sub(1, Ordering::Relaxed);
                            }
                        }
                        self.tree.replace_or_insert(item);
                    }
                    Operation::Merge => {
                        if item.is_tombstone() {
                            let info = &mut self.store_info.lock();
                            let object_count = &mut info.object_count;
                            *object_count = object_count.saturating_sub(1);
                        }
                        if !context.mode.is_replay()
                            && matches!(
                                item.key.data,
                                ObjectKeyData::GraveyardEntry { .. }
                                    | ObjectKeyData::GraveyardAttributeEntry { .. }
                            )
                        {
                            if matches!(item.value, ObjectValue::Some | ObjectValue::Trim) {
                                self.graveyard_entries.fetch_add(1, Ordering::Relaxed);
                            } else if matches!(item.value, ObjectValue::None) {
                                self.graveyard_entries.fetch_sub(1, Ordering::Relaxed);
                            }
                        }
                        let lower_bound = item.key.key_for_merge_into();
                        self.tree.merge_into(item, &lower_bound);
                    }
                }
            }
            Mutation::BeginFlush => {
                ensure!(self.parent_store.is_some(), FxfsError::Inconsistent);
                self.tree.seal();
            }
            Mutation::EndFlush => ensure!(self.parent_store.is_some(), FxfsError::Inconsistent),
            Mutation::CreateInternalDir(object_id) => {
                ensure!(object_id != INVALID_OBJECT_ID, FxfsError::Inconsistent);
                self.store_info.lock().internal_directory_object_id = object_id;
            }
            _ => bail!("unexpected mutation: {:?}", mutation),
        }
        Ok(())
    }

    fn drop_mutation(&self, _mutation: Mutation, _transaction: &Transaction<'_>) {}

    /// Push all in-memory structures to the device. This is not necessary for sync since the
    /// journal will take care of it.  This is supposed to be called when there is either memory or
    /// space pressure (flushing the store will persist in-memory data and allow the journal file to
    /// be trimmed).
    ///
    /// Also returns the earliest version of a struct in the filesystem (when known).
    async fn flush(&self, reason: FlushReason) -> Result<Version, Error> {
        self.flush_with_reason(reason).await
    }
}

impl HandleOwner for ObjectStore {}

impl AsRef<ObjectStore> for ObjectStore {
    fn as_ref(&self) -> &ObjectStore {
        self
    }
}

impl AssociatedObject for ObjectStore {}

/// Argument to the trim_some method.
#[derive(Debug)]
pub enum TrimMode {
    /// Trim extents beyond the current size.
    UseSize,

    /// Trim extents beyond the supplied offset.
    FromOffset(u64),

    /// Remove the object (or attribute) from the store once it is fully trimmed.
    Tombstone(TombstoneMode),
}

/// Sets the mode for tombstoning (either at the object or attribute level).
#[derive(Debug)]
pub enum TombstoneMode {
    Object,
    Attribute,
}

/// Result of the trim_some method.
#[derive(Debug)]
pub enum TrimResult {
    /// We reached the limit of the transaction and more extents might follow.
    Incomplete,

    /// We finished this attribute.  Returns the ID of the next attribute for the same object if
    /// there is one.
    Done(Option<AttributeId>),
}

/// Loads store info.
pub async fn load_store_info(
    parent: &Arc<ObjectStore>,
    store_object_id: u64,
) -> Result<StoreInfo, Error> {
    load_store_info_from_handle(
        &ObjectStore::open_object(parent, store_object_id, HandleOptions::default()).await?,
    )
    .await
}

async fn load_store_info_from_handle(
    handle: &DataObjectHandle<impl HandleOwner>,
) -> Result<StoreInfo, Error> {
    Ok(if handle.get_size() > 0 {
        let serialized_info = handle.contents(MAX_STORE_INFO_SERIALIZED_SIZE).await?;
        let mut cursor = std::io::Cursor::new(serialized_info);
        let (store_info, version) = StoreInfo::deserialize_with_version(&mut cursor)
            .context("Failed to deserialize StoreInfo")?;
        // cladefs: refuse StoreInfo outside EARLIEST_SUPPORTED_VERSION..=LATEST_VERSION (doc-28,
        // board 10050).
        ensure!(
            (EARLIEST_SUPPORTED_VERSION..=LATEST_VERSION).contains(&version),
            anyhow!(FxfsError::InvalidVersion)
                .context(format!("Unsupported StoreInfo version: {version:?}"))
        );
        store_info
    } else {
        // The store_info will be absent for a newly created and empty object store.
        StoreInfo::default()
    })
}

#[cfg(test)]
mod tests {
    use super::{
        AttributeId, HandleOptions, LockKey, MAX_STORE_INFO_SERIALIZED_SIZE, Mutation,
        NewChildStoreOptions, ObjectStore, StoreInfo,
    };
    use crate::errors::FxfsError;
    use crate::filesystem::{FxFilesystem, FxFilesystemBuilder, OpenFxFilesystem};
    use crate::fsck::fsck;
    use crate::hooks::HooksHandle;
    use crate::lsm_tree::Query;
    use crate::lsm_tree::types::{ItemRef, LayerIterator};
    use crate::object_handle::{INVALID_OBJECT_ID, ObjectHandle, WriteObjectHandle};
    use crate::object_store::directory::Directory;
    use crate::object_store::object_record::{ObjectKey, ObjectValue};
    use crate::object_store::store_object_handle::MAX_INLINE_XATTR_SIZE;
    use crate::object_store::transaction::{LockKeys, Options, lock_keys};
    use crate::object_store::volume::root_volume;
    use crate::serialized_types::{DEFAULT_MAX_SERIALIZED_RECORD_SIZE, Versioned, VersionedLatest};
    use fuchsia_async as fasync;
    use fuchsia_sync::Mutex;
    use futures::join;
    use std::sync::Arc;
    use std::time::Duration;
    use storage_device::DeviceHolder;
    use storage_device::fake_device::FakeDevice;

    const TEST_DEVICE_BLOCK_SIZE: u32 = 512;

    async fn test_filesystem_with_hooks(hooks: Arc<HooksHandle>) -> OpenFxFilesystem {
        let device = DeviceHolder::new(FakeDevice::new(8192, TEST_DEVICE_BLOCK_SIZE));
        FxFilesystemBuilder::new()
            .hooks(hooks)
            .format(true)
            .open(device)
            .await
            .expect("open failed")
    }

    async fn test_filesystem() -> OpenFxFilesystem {
        test_filesystem_with_hooks(Default::default()).await
    }

    #[fuchsia_async::run(10, test)]
    async fn test_old_layers_are_purged() {
        let fs = test_filesystem().await;

        let store = fs.root_store();
        let mut transaction = fs
            .root_store()
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new_transaction failed");
        let object = Arc::new(
            ObjectStore::create_object(&store, &mut transaction, HandleOptions::default())
                .await
                .expect("create_object failed"),
        );
        transaction.commit().await.expect("commit failed");

        store.flush().await.expect("flush failed");

        let mut buf = object.allocate_buffer(5).await;
        buf.copy_from_slice(b"hello");
        object.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");

        // Getting the layer-set should cause the flush to stall.
        let layer_set = store.tree().layer_set();

        let done = Mutex::new(false);
        let mut object_id = 0;

        join!(
            async {
                store.flush().await.expect("flush failed");
                assert!(*done.lock());
            },
            async {
                // This is a halting problem so all we can do is sleep.
                fasync::Timer::new(Duration::from_secs(1)).await;
                *done.lock() = true;
                object_id = layer_set.layers.last().unwrap().handle().unwrap().object_id();
                std::mem::drop(layer_set);
            }
        );

        if let Err(e) = ObjectStore::open_object(
            &store.parent_store.as_ref().unwrap(),
            object_id,
            HandleOptions::default(),
        )
        .await
        {
            assert!(FxfsError::NotFound.matches(&e));
        } else {
            panic!("open_object succeeded");
        }
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_tombstone_deletes_data() {
        let fs = test_filesystem().await;
        let root_store = fs.root_store();
        let child_id = {
            let mut transaction = fs
                .root_store()
                .new_transaction(lock_keys![], Options::default())
                .await
                .expect("new_transaction failed");
            let child =
                ObjectStore::create_object(&root_store, &mut transaction, HandleOptions::default())
                    .await
                    .expect("create_object failed");
            root_store.add_to_graveyard(&mut transaction, child.object_id());
            transaction.commit().await.expect("commit failed");

            // Allocate an extent in the file.
            let mut buffer = child.allocate_buffer(8192).await;
            buffer.fill(0xaa);
            child.write_or_append(Some(0), buffer.as_ref()).await.expect("write failed");

            child.object_id()
        };

        root_store
            .tombstone_object(child_id, Options::default(), None)
            .await
            .expect("tombstone failed");

        // Let fsck check allocations.
        fsck(fs.clone()).await.expect("fsck failed");
    }

    /// cladefs: a store's information may say it has no graveyard, which what adds to one asserts it
    /// has, so it doesn't open.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_store_with_no_graveyard() {
        let fs = test_filesystem().await;
        let store = fs.root_store();
        // Declared first, as the transaction writes it.
        let handle;
        let mut transaction = store
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new_transaction failed");
        handle = ObjectStore::create_object(&store, &mut transaction, HandleOptions::default())
            .await
            .expect("create_object failed");
        let mut serialized = Vec::new();
        StoreInfo::default().serialize_with_version(&mut serialized).expect("serializing");
        let mut buf = handle.allocate_buffer(serialized.len()).await;
        buf.as_mut().copy_from_slice(&serialized);
        handle.txn_write(&mut transaction, 0u64, buf.as_ref()).await.expect("txn_write failed");
        transaction.commit().await.expect("commit failed");
        let object_id = handle.object_id();
        let error = ObjectStore::open(&store, object_id, None).await.map(drop).expect_err("opened");
        assert!(FxfsError::Inconsistent.matches(&error), "{error:?}");
        fs.close().await.expect("close failed");
    }

    /// cladefs: what a graveyard holds is what an image says: an object that isn't there, or isn't
    /// in it, fails its tombstone, where a mount reaps it, and panics nothing.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_tombstone_of_what_a_graveyard_should_not_hold() {
        let fs = test_filesystem().await;
        let store = fs.root_store();
        let error = store
            .tombstone_object(1 << 40, Options::default(), None)
            .await
            .expect_err("tombstoned what isn't there");
        assert!(FxfsError::Inconsistent.matches(&error), "{error:?}");

        let mut transaction = store
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new_transaction failed");
        let handle = ObjectStore::create_object(&store, &mut transaction, HandleOptions::default())
            .await
            .expect("create_object failed");
        transaction.commit().await.expect("commit failed");
        let error = store
            .tombstone_object(handle.object_id(), Options::default(), None)
            .await
            .expect_err("tombstoned what isn't in the graveyard");
        assert!(FxfsError::Inconsistent.matches(&error), "{error:?}");

        // A record that isn't an object's, as a corrupt layer's may be.
        let object_id = handle.object_id();
        let keys = lock_keys![LockKey::object(store.store_object_id(), object_id)];
        let mut transaction =
            store.new_transaction(keys, Options::default()).await.expect("new_transaction failed");
        store.add_to_graveyard(&mut transaction, object_id);
        transaction.add(
            store.store_object_id(),
            Mutation::replace_or_insert_object(ObjectKey::object(object_id), ObjectValue::Some),
        );
        transaction.commit().await.expect("commit failed");
        store
            .tombstone_object(object_id, Options::default(), None)
            .await
            .expect_err("tombstoned what isn't an object");
        drop(handle);
        fs.close().await.expect("close failed");
    }

    /// cladefs: an unlinked object is tombstoned in its caller's transaction, whatever its graveyard
    /// entry is (the transaction's own, the tree's, or both, the tree's a trim's), taking the entry
    /// with it, which the graveyard's count says too; one with attributes other than its data
    /// stays in the graveyard, and one not in it isn't tombstoned.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_txn_tombstone_unlinked() {
        let fs = test_filesystem().await;
        let store = fs.root_store();
        let store_id = store.store_object_id();
        let entry = |object_id| {
            ObjectKey::graveyard_entry(store.graveyard_directory_object_id(), object_id)
        };
        let mut transaction = store
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new_transaction failed");
        let mut handles = Vec::new();
        for _ in 0..5 {
            handles.push(
                ObjectStore::create_object(&store, &mut transaction, HandleOptions::default())
                    .await
                    .expect("create_object failed"),
            );
        }
        transaction.commit().await.expect("commit failed");
        let [empty, written, trimmed, two_attributes, named] = handles.try_into().ok().unwrap();
        for handle in [&written, &trimmed, &two_attributes] {
            let mut buffer = handle.allocate_buffer(8192).await;
            buffer.fill(0xaa);
            handle.write_or_append(Some(0), buffer.as_ref()).await.expect("write failed");
        }
        two_attributes.write_attr(AttributeId::TEST_ID, b"bar").await.expect("write_attr failed");
        let ids = [&empty, &written, &trimmed, &two_attributes, &named].map(|h| h.object_id());
        let [empty, written, trimmed, two_attributes, named] = ids;

        // Before: the empty object's entry and the trim of another in the tree.
        let graveyard_count = store.graveyard_count();
        let mut transaction = store
            .new_transaction(lock_keys![LockKey::object(store_id, empty)], Options::default())
            .await
            .expect("new_transaction failed");
        store.add_to_graveyard(&mut transaction, empty);
        transaction
            .add(store_id, Mutation::replace_or_insert_object(entry(trimmed), ObjectValue::Trim));
        transaction.commit().await.expect("commit failed");
        assert_eq!(store.graveyard_count(), graveyard_count + 2);

        // The unlinks of the others' last names put them in the graveyard in the transaction that
        // tombstones them.
        let mut keys = LockKeys::with_capacity(2 * ids.len());
        for object_id in ids {
            keys.push(LockKey::object(store_id, object_id));
            keys.push(LockKey::object_attribute(store_id, object_id, AttributeId::SORTED_START));
        }
        let mut transaction =
            store.new_transaction(keys, Options::default()).await.expect("new_transaction failed");
        for object_id in [written, trimmed, two_attributes] {
            assert!(store.adjust_refs(&mut transaction, object_id, -1).await.expect("adjust_refs"));
        }
        for object_id in [empty, written, trimmed] {
            assert!(
                store
                    .txn_tombstone_unlinked(&mut transaction, object_id)
                    .await
                    .expect("tombstone failed"),
                "{object_id} not tombstoned"
            );
        }
        assert!(
            !store
                .txn_tombstone_unlinked(&mut transaction, two_attributes)
                .await
                .expect("tombstone failed")
        );
        let error = store
            .txn_tombstone_unlinked(&mut transaction, named)
            .await
            .expect_err("tombstoned what isn't in the graveyard");
        assert!(FxfsError::Inconsistent.matches(&error), "{error:?}");
        transaction.commit().await.expect("commit failed");

        for object_id in [empty, written, trimmed] {
            assert!(store.tree().find(&entry(object_id)).await.expect("find").is_none());
            let error = ObjectStore::open_object(&store, object_id, HandleOptions::default())
                .await
                .err()
                .expect("open_object succeeded");
            assert!(FxfsError::NotFound.matches(&error), "{error:?}");
        }
        assert!(store.tree().find(&entry(two_attributes)).await.expect("find").is_some());
        assert_eq!(store.graveyard_count(), graveyard_count + 1);
        // Let fsck check allocations, and that no entry is left of an object gone.
        fsck(fs.clone()).await.expect("fsck failed");

        // The graveyard finishes what it left. (Its count isn't checked: `trim_or_tombstone`
        // removes an attribute's entry there never was, which it counts.)
        store
            .tombstone_object(two_attributes, Options::default(), None)
            .await
            .expect("tombstone failed");
        assert!(store.tree().find(&entry(two_attributes)).await.expect("find").is_none());
        fsck(fs.clone()).await.expect("fsck failed");
        fs.close().await.expect("close failed");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_major_compaction_discards_unnecessary_records() {
        let fs = test_filesystem().await;
        let root_store = fs.root_store();
        let child_id = {
            let mut transaction = fs
                .root_store()
                .new_transaction(lock_keys![], Options::default())
                .await
                .expect("new_transaction failed");
            let child =
                ObjectStore::create_object(&root_store, &mut transaction, HandleOptions::default())
                    .await
                    .expect("create_object failed");
            root_store.add_to_graveyard(&mut transaction, child.object_id());
            transaction.commit().await.expect("commit failed");

            // Allocate an extent in the file.
            let mut buffer = child.allocate_buffer(8192).await;
            buffer.fill(0xaa);
            child.write_or_append(Some(0), buffer.as_ref()).await.expect("write failed");

            child.object_id()
        };

        root_store
            .tombstone_object(child_id, Options::default(), None)
            .await
            .expect("tombstone failed");
        {
            let layers = root_store.tree.layer_set();
            let mut merger = layers.merger();
            let iter = merger
                .query(Query::FullRange(&ObjectKey::object(child_id)))
                .await
                .expect("seek failed");
            // Find at least one object still in the tree.
            match iter.get() {
                Some(ItemRef { key: ObjectKey { object_id, .. }, .. })
                    if *object_id == child_id => {}
                _ => panic!("Objects should still be in the tree."),
            }
        }
        root_store.flush().await.expect("flush failed");

        // There should be no records for the object.
        let layers = root_store.tree.layer_set();
        let mut merger = layers.merger();
        let iter = merger
            .query(Query::FullRange(&ObjectKey::object(child_id)))
            .await
            .expect("seek failed");
        match iter.get() {
            None => {}
            Some(ItemRef { key: ObjectKey { object_id, .. }, .. }) => {
                assert_ne!(*object_id, child_id)
            }
        }
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_overlapping_extents_in_different_layers() {
        let fs = test_filesystem().await;
        let store = fs.root_store();

        let mut transaction = store
            .new_transaction(
                lock_keys![LockKey::object(
                    store.store_object_id(),
                    store.root_directory_object_id()
                )],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        let root_directory =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");
        let object = root_directory
            .create_child_file(&mut transaction, "test")
            .await
            .expect("create_child_file failed");
        transaction.commit().await.expect("commit failed");

        let buf = object.allocate_buffer(16384).await;
        object.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");

        store.flush().await.expect("flush failed");

        object.write_or_append(Some(0), buf.subslice(0..4096)).await.expect("write failed");

        // At this point, we should have an extent for 0..16384 in a layer that has been flushed,
        // and an extent for 0..4096 that partially overwrites it.  Writing to 0..16384 should
        // overwrite both of those extents.
        object.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");

        fsck(fs.clone()).await.expect("fsck failed");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_object_ids_carry_past_32_bits() {
        let fs = test_filesystem().await;

        {
            let root_volume = root_volume(fs.clone()).await.expect("root_volume failed");
            let store = root_volume
                .new_volume("test", NewChildStoreOptions::default())
                .await
                .expect("new_volume failed");

            // Hack the last object ID.
            {
                let mut last_object_id = store.last_object_id.lock();
                assert_eq!(*last_object_id >> 32, 0);
                *last_object_id |= 0xffffffff;
            }

            let mut transaction = store
                .new_transaction(
                    lock_keys![LockKey::object(
                        store.store_object_id(),
                        store.root_directory_object_id()
                    )],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            let root_directory = Directory::open(&store, store.root_directory_object_id())
                .await
                .expect("open failed");
            let object = root_directory
                .create_child_file(&mut transaction, "test")
                .await
                .expect("create_child_file failed");
            transaction.commit().await.expect("commit failed");

            assert_eq!(object.object_id(), 0x1_0000_0000);

            assert_eq!(*store.last_object_id.lock(), 0x1_0000_0000);
        };

        fs.close().await.expect("Close failed");
        let device = fs.take_device().await;
        device.reopen(false);
        let fs = FxFilesystem::open(device).await.expect("open failed");
        let root_volume = root_volume(fs.clone()).await.expect("root_volume failed");
        let store = root_volume.volume("test").await.expect("volume failed");

        assert_eq!(*store.last_object_id.lock(), 0x1_0000_0000);
    }

    #[test]
    fn test_store_info_max_serialized_size() {
        let info = StoreInfo {
            guid: [0xff; 16],
            last_object_id: 0x1234567812345678,
            // Worst case, each layer should be 3/4 the size of the layer below it (because of the
            // compaction policy we're using).  If the smallest layer is 8,192 bytes, then 120
            // layers would take up a size that exceeds a 64 bit unsigned integer, so if this fits,
            // any size should fit.
            layers: vec![0x1234567812345678; 120],
            root_directory_object_id: 0x1234567812345678,
            graveyard_directory_object_id: 0x1234567812345678,
            object_count: 0x1234567812345678,
            internal_directory_object_id: INVALID_OBJECT_ID,
            base_layers: Vec::new(),
            base_cells: Vec::new(),
        };
        let mut serialized_info = Vec::new();
        info.serialize_with_version(&mut serialized_info).unwrap();
        assert!(
            serialized_info.len() <= MAX_STORE_INFO_SERIALIZED_SIZE,
            "{}",
            serialized_info.len()
        );
    }

    #[fuchsia_async::run(10, test)]
    #[should_panic(expected = "Insufficient reservation space")]
    #[cfg(debug_assertions)]
    async fn large_transaction_causes_panic_in_debug_builds() {
        let fs = test_filesystem().await;
        let root_volume = root_volume(fs.clone()).await.expect("root_volume failed");
        let store = root_volume
            .new_volume("vol", NewChildStoreOptions::default())
            .await
            .expect("new_volume failed");
        let root_directory =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");
        let mut transaction = fs
            .root_store()
            .new_transaction(
                lock_keys![LockKey::object(store.store_object_id(), root_directory.object_id())],
                Options::default(),
            )
            .await
            .expect("transaction");
        for i in 0..500 {
            root_directory
                .create_symlink(&mut transaction, b"link", &format!("{}", i))
                .await
                .expect("symlink");
        }
        assert_eq!(transaction.commit().await.expect("commit"), 0);
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_large_mutations_in_transaction() {
        let fs = test_filesystem().await;

        let root_volume = root_volume(fs.clone()).await.expect("root_volume failed");
        let store = root_volume
            .new_volume("test", NewChildStoreOptions::default())
            .await
            .expect("new_volume failed");
        let store_object_id = store.store_object_id();
        let root_dir_id = store.root_directory_object_id();

        let root_dir = Directory::open(&store, root_dir_id).await.expect("open root dir failed");
        let mut transaction = store
            .new_transaction(
                lock_keys![LockKey::object(store_object_id, root_dir_id)],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");

        let file = root_dir
            .create_child_file(&mut transaction, "test_file")
            .await
            .expect("create_child_file failed");

        // Add multiple inline extended attributes to the file, limiting each to
        // MAX_INLINE_XATTR_SIZE, until the total serialized size exceeds
        // DEFAULT_MAX_SERIALIZED_RECORD_SIZE.
        let mut total_size = 0;
        let mut i = 0;
        while total_size <= DEFAULT_MAX_SERIALIZED_RECORD_SIZE {
            let name = format!("attr{i}").into_bytes();
            let mutation = Mutation::replace_or_insert_object(
                ObjectKey::extended_attribute(file.object_id(), name),
                ObjectValue::inline_extended_attribute(vec![0u8; MAX_INLINE_XATTR_SIZE]),
            );
            let mut buf = Vec::new();
            mutation.serialize_into(&mut buf).unwrap();
            total_size += buf.len() as u64;
            transaction.add(store_object_id, mutation);
            i += 1;
        }

        transaction.commit().await.expect("commit should succeed");

        for j in 0..i {
            let name = format!("attr{j}").into_bytes();
            let item = store
                .tree
                .find(&ObjectKey::extended_attribute(file.object_id(), name))
                .await
                .expect("find failed")
                .expect("attr not found");
            assert_eq!(
                item.value,
                ObjectValue::inline_extended_attribute(vec![0u8; MAX_INLINE_XATTR_SIZE])
            );
        }
    }
}
