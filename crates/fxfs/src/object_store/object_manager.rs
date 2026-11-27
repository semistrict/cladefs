// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::errors::FxfsError;
use crate::filesystem::{ApplyContext, ApplyMode, FlushReason, ForceMajor, JournalingObject};
use crate::log::*;
use crate::object_handle::INVALID_OBJECT_ID;
use crate::object_store::allocator::{Allocator, Reservation};
use crate::object_store::directory::Directory;
use crate::object_store::journal::{self, JournalCheckpoint};
use crate::object_store::transaction::{
    AssocObj, AssociatedObject, MetadataReservation, Mutation, Transaction, TxnMutation, lock_keys,
};
use crate::object_store::volume::{VOLUMES_DIRECTORY, list_volumes};
use crate::object_store::{LockKey, ObjectDescriptor, ObjectStore};
use crate::serialized_types::{LATEST_VERSION, Version};
use anyhow::{Context, Error, anyhow, bail, ensure};
use fuchsia_sync::RwLock;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::collections::hash_map::Entry;
use std::num::Saturating;
use std::sync::{Arc, OnceLock};
use storage_units::BlockSize;

// Data written to the journal eventually needs to be flushed somewhere (typically into layer
// files).  Here we conservatively assume that could take up to four times as much space as it does
// in the journal.  In the layer file, it'll take up at least as much, but we must reserve the same
// again that so that there's enough space for compactions, and then we need some spare for
// overheads.
//
// TODO(https://fxbug.dev/42178158): We should come up with a better way of determining what the multiplier
// should be here.  2x was too low, as it didn't cover any space for metadata.  4x might be too
// much.
pub const fn reserved_space_from_journal_usage(journal_usage: u64) -> u64 {
    journal_usage * 4
}

/// cladefs: what flushing an object the journal holds updates of takes of the metadata reservation,
/// in the filesystem's blocks, however little the journal holds of it (board 14243). A store's
/// flush writes a new layer file of at least three blocks (a header, a block of items and the
/// layer's info: `MINIMUM_LAYER_FILE_BLOCKS` and one), which a new store's first flush takes of
/// the reservation whole (measured: 12 KiB each); a later one gets most of it back as its old
/// layers go. The flush journals some 380 bytes of records of its own (the new layer, its extents,
/// its graveyard entries, the store's info rewritten), which the flushes of the root store and the
/// allocator then write into their layers: the fourth block. Four times what an object takes of
/// the journal (`reserved_space_from_journal_usage`) pays for what its flush writes in proportion
/// to that, not this: 300 volumes created, a few hundred bytes of the journal apiece, compacted
/// at once, needed nearly three times what the reservation held, and failed with ENOSPC on a mostly empty
/// filesystem. Upstream reserves nothing for it: its stores are few. (hotbed question 585)
const FLUSH_COST_BLOCKS: u64 = 4;

/// cladefs: what each of `Inner::required_parts` is for.
const REQUIRED_PARTS: [&str; 4] = ["compaction", "journal", "reserved", "flush charges"];

/// ObjectManager is a global loading cache for object stores and other special objects.
pub struct ObjectManager {
    inner: RwLock<Inner>,
    metadata_reservation: OnceLock<Reservation>,
    volume_directory: OnceLock<Directory<ObjectStore>>,
    on_new_store: Option<Box<dyn Fn(&ObjectStore) + Send + Sync>>,
}

// Whilst we are flushing we need to keep track of the old checkpoint that we are hoping to flush,
// and a new one that should apply if we successfully finish the flush.
#[derive(Debug)]
enum Checkpoints {
    Current(JournalCheckpoint),
    Old(JournalCheckpoint),
    Both(/* old: */ JournalCheckpoint, /* current: */ JournalCheckpoint),
}

impl Checkpoints {
    // Returns the earliest checkpoint (which will always be the old one if present).
    fn earliest(&self) -> &JournalCheckpoint {
        match self {
            Checkpoints::Old(x) | Checkpoints::Both(x, _) | Checkpoints::Current(x) => x,
        }
    }
}

// We currently maintain strong references to all stores that have been opened, but there's no
// currently no mechanism for releasing stores that aren't being used.
struct Inner {
    stores: HashMap<u64, Arc<ObjectStore>>,
    root_parent_store_object_id: u64,
    root_store_object_id: u64,
    allocator_object_id: u64,
    allocator: Option<Arc<Allocator>>,

    // Records dependencies on the journal for objects i.e. an entry for object ID 1, would mean it
    // has a dependency on journal records from that offset.
    journal_checkpoints: HashMap<u64, Checkpoints>,

    // Mappings from object-id to a target reservation amount.  The object IDs here are from the
    // root store namespace, so it can be associated with any object in the root store.  A
    // reservation will be made to cover the *maximum* in this map, since it is assumed that any
    // requirement is only temporary, for the duration of a compaction, and that once compaction has
    // finished for a particular object, the space will be recovered.
    reservations: HashMap<u64, u64>,

    // The last journal end offset for a transaction that has been applied.  This is not necessarily
    // the same as the start offset for the next transaction because of padding.
    last_end_offset: u64,

    // cladefs: what the last transaction applied took of the journal, as it was charged: from the
    // last's end, past the padding before it that the journal's own transaction accounted for.
    last_journal_usage: u64,

    // cladefs: the journal's blocks unfreed, as of the last commit (`Journal::journal_bytes`): the
    // room the reserved space (`reserved_space`) is for, taken already (board 14491).
    journal_held: u64,

    // A running counter that tracks metadata space that has been borrowed on the understanding that
    // eventually it will be recovered (potentially after a full compaction).
    borrowed_metadata_space: u64,

    // cladefs: `borrowed_metadata_space` as replay found it, before the mount's own transactions: what
    // the filesystem had borrowed when it stopped, if nothing was lost (TASK-218).
    borrowed_as_replayed: u64,

    // Extra temporary space that might be tied up in the journal that hasn't yet been deallocated.
    reserved_space: u64,

    // cladefs: what flushing each object in `journal_checkpoints` costs, whatever it holds
    // (`FLUSH_COST_BLOCKS`).
    flush_cost: u64,
}

impl Inner {
    fn earliest_journal_offset(&self) -> Option<u64> {
        self.journal_checkpoints.values().map(|c| c.earliest().file_offset).min()
    }

    /// cladefs: what the metadata reservation holds for flushing the objects the journal holds
    /// updates of, whatever they are (`FLUSH_COST_BLOCKS`).
    fn flush_charges(&self) -> u64 {
        self.flush_cost * self.journal_checkpoints.len() as u64
    }

    /// cladefs: whether applying `mutation` to `object_id` leaves the journal holding an update of
    /// it, which it must flush: a checkpoint in `journal_checkpoints`.
    fn dirties(&self, object_id: u64, mutation: &Mutation) -> bool {
        !matches!(mutation, Mutation::BeginFlush | Mutation::EndFlush | Mutation::DeleteVolume)
            && object_id != self.root_parent_store_object_id
    }

    /// cladefs: what flushing the objects `mutations` dirty first (`dirties`), that the journal
    /// holds no update of yet, adds to the metadata reservation.
    fn flush_charge<'a, 'b: 'a>(
        &self,
        mutations: impl IntoIterator<Item = &'a TxnMutation<'b>>,
    ) -> u64 {
        let dirtied: HashSet<u64> = mutations
            .into_iter()
            .filter(|m| {
                self.dirties(m.object_id, &m.mutation)
                    && !self.journal_checkpoints.contains_key(&m.object_id)
            })
            .map(|m| m.object_id)
            .collect();
        self.flush_cost * dirtied.len() as u64
    }

    // Returns the required size of the metadata reservation assuming that no space has been
    // borrowed.  The invariant is: reservation-size + borrowed-space = required.
    fn required_reservation(&self) -> u64 {
        self.required_parts().iter().sum()
    }

    /// cladefs: the parts of [`Self::required_reservation`], in the order [`REQUIRED_PARTS`] names
    /// them, which a mount that can't reserve them says (board 14491).
    fn required_parts(&self) -> [u64; 4] {
        [
            // Start with the maximum amount of temporary space we might need during compactions.
            *self.reservations.values().max().unwrap_or(&0),
            // Account for data that has been written to the journal that will need to be written
            // to layer files when flushed. cladefs: there is none if the earliest is ahead of
            // what's been accounted for, as a mount's renewal of the journal's tail, its first
            // transaction, is of a journal compacted clean, with what a replay discarded behind
            // it.
            self.earliest_journal_offset()
                .map(|min| {
                    reserved_space_from_journal_usage(self.last_end_offset.saturating_sub(min))
                })
                .unwrap_or(0),
            // Extra reserved space. cladefs: room for the journal's blocks, which upstream's journal
            // allocated from the metadata reservation; cladefs's are system extents beside it, which
            // the allocator counts used themselves: what they take already the reservation holds
            // no more, or a filesystem running holds the room twice, and its mount, counting both,
            // finds it short (board 14491).
            self.reserved_space.saturating_sub(self.journal_held),
            // cladefs: and flushing each object, whatever it holds.
            self.flush_charges(),
        ]
    }

    fn journaling_object(&self, object_id: u64) -> Option<Arc<dyn JournalingObject>> {
        if object_id == self.allocator_object_id {
            Some(self.allocator.clone().unwrap() as Arc<dyn JournalingObject>)
        } else {
            self.stores.get(&object_id).map(|x| x.clone() as Arc<dyn JournalingObject>)
        }
    }
}

impl ObjectManager {
    /// cladefs: for a filesystem of `block_size` blocks, which flushing an object costs
    /// (`FLUSH_COST_BLOCKS`).
    pub fn new(
        block_size: BlockSize,
        on_new_store: Option<Box<dyn Fn(&ObjectStore) + Send + Sync>>,
    ) -> ObjectManager {
        ObjectManager {
            inner: RwLock::new(Inner {
                stores: HashMap::default(),
                root_parent_store_object_id: INVALID_OBJECT_ID,
                root_store_object_id: INVALID_OBJECT_ID,
                allocator_object_id: INVALID_OBJECT_ID,
                allocator: None,
                journal_checkpoints: HashMap::default(),
                reservations: HashMap::default(),
                last_end_offset: 0,
                last_journal_usage: 0,
                journal_held: 0,
                borrowed_metadata_space: 0,
                borrowed_as_replayed: 0,
                reserved_space: journal::RESERVED_SPACE,
                flush_cost: block_size * FLUSH_COST_BLOCKS,
            }),
            metadata_reservation: OnceLock::new(),
            volume_directory: OnceLock::new(),
            on_new_store,
        }
    }

    pub fn required_reservation(&self) -> u64 {
        self.inner.read().required_reservation()
    }

    pub fn root_parent_store_object_id(&self) -> u64 {
        self.inner.read().root_parent_store_object_id
    }

    pub fn root_parent_store(&self) -> Arc<ObjectStore> {
        let inner = self.inner.read();
        inner.stores.get(&inner.root_parent_store_object_id).unwrap().clone()
    }

    pub fn set_root_parent_store(&self, store: Arc<ObjectStore>) {
        if let Some(on_new_store) = &self.on_new_store {
            on_new_store(&store);
        }
        let mut inner = self.inner.write();
        let store_id = store.store_object_id();
        inner.stores.insert(store_id, store);
        inner.root_parent_store_object_id = store_id;
    }

    pub fn root_store_object_id(&self) -> u64 {
        self.inner.read().root_store_object_id
    }

    pub fn root_store(&self) -> Arc<ObjectStore> {
        let inner = self.inner.read();
        inner.stores.get(&inner.root_store_object_id).unwrap().clone()
    }

    pub fn set_root_store(&self, store: Arc<ObjectStore>) {
        if let Some(on_new_store) = &self.on_new_store {
            on_new_store(&store);
        }
        let mut inner = self.inner.write();
        let store_id = store.store_object_id();
        inner.stores.insert(store_id, store);
        inner.root_store_object_id = store_id;
    }

    pub fn is_system_store(&self, store_id: u64) -> bool {
        let inner = self.inner.read();
        store_id == inner.root_store_object_id || store_id == inner.root_parent_store_object_id
    }

    /// Returns the store which might or might not be locked.
    pub fn store(&self, store_object_id: u64) -> Option<Arc<ObjectStore>> {
        self.inner.read().stores.get(&store_object_id).cloned()
    }

    /// Returns the total bytes written to the LSM trees in the allocator and all object stores
    /// during compaction operations.
    pub fn compaction_bytes_written(&self) -> u64 {
        let inner = self.inner.read();
        let mut total = 0;
        if let Some(allocator) = &inner.allocator {
            total += allocator.tree().compaction_bytes_written();
        }
        for store in inner.stores.values() {
            total += store.tree().compaction_bytes_written();
        }
        total
    }

    /// cladefs: total bytes currently held in persistent (and base) layer read chunk caches across
    /// the allocator and every open object store (TASK-596).
    pub fn cached_layer_bytes(&self) -> usize {
        let inner = self.inner.read();
        let mut total = 0;
        if let Some(allocator) = &inner.allocator {
            total += allocator.tree().cached_layer_bytes();
        }
        for store in inner.stores.values() {
            total += store.tree().cached_layer_bytes();
        }
        total
    }

    /// cladefs: total number of persistent (and base) layer read chunks currently cached across
    /// the allocator and every open object store (TASK-596). Non-blocking and allocation-free so
    /// direct reclaim (`super_cache_count` -> `Fxfs::cached`) during an allocation under
    /// `self.inner`, an `LSMTree::data`, or a layer's chunk lock cannot self-deadlock (TASK-292,
    /// hotbed review 109).
    pub fn cached_layer_chunks(&self) -> usize {
        let Some(inner) = self.inner.try_read() else {
            return 0;
        };
        let mut total = 0;
        if let Some(allocator) = &inner.allocator {
            total += allocator.tree().cached_layer_chunks();
        }
        for store in inner.stores.values() {
            total += store.tree().cached_layer_chunks();
        }
        total
    }

    /// cladefs: non-blocking, allocation-free eviction of up to `max_chunks` persistent (and base)
    /// layer read chunks across every open object store and the allocator, returning how many were
    /// evicted (TASK-292, TASK-596, hotbed review 109).
    pub fn evict_layer_chunks(&self, mut max_chunks: usize) -> usize {
        if max_chunks == 0 {
            return 0;
        }
        let Some(inner) = self.inner.try_read() else {
            return 0;
        };
        let mut evicted = 0;
        for store in inner.stores.values() {
            if max_chunks == 0 {
                return evicted;
            }
            let n = store.tree().evict_layer_chunks(max_chunks);
            evicted += n;
            max_chunks -= n;
        }
        if max_chunks > 0
            && let Some(allocator) = &inner.allocator
        {
            evicted += allocator.tree().evict_layer_chunks(max_chunks);
        }
        evicted
    }

    /// This is not thread-safe: it assumes that a store won't be forgotten whilst the loop is
    /// running.  This is to be used after replaying the journal.
    pub async fn on_replay_complete(&self) -> Result<(), Error> {
        let root_store = self.root_store();

        let root_directory = Directory::open(&root_store, root_store.root_directory_object_id())
            .await
            .context("Unable to open root volume directory")?;

        match root_directory.lookup(VOLUMES_DIRECTORY).await? {
            None => bail!("Root directory not found"),
            Some((object_id, ObjectDescriptor::Directory)) => {
                let volume_directory = Directory::open(&root_store, object_id)
                    .await
                    .context("Unable to open volumes directory")?;
                self.volume_directory.set(volume_directory).unwrap();
            }
            _ => {
                bail!(
                    anyhow!(FxfsError::Inconsistent)
                        .context("Unexpected type for volumes directory")
                )
            }
        }

        // cladefs: replay opened those with something in the journal, which lazily are all that open.
        if !root_store.filesystem().options().lazy_volumes {
            let object_ids = list_volumes(self.volume_directory.get().unwrap())
                .await
                .context("Failed to list volumes")?;

            for store_id in object_ids {
                self.open_store(&root_store, store_id).await?;
            }
        }

        {
            let mut inner = self.inner.write();
            inner.borrowed_as_replayed = inner.borrowed_metadata_space;
        }

        // This can fail if a filesystem is created and truncated to a size
        // that doesn't leave enough free space for metadata reservations.
        self.init_metadata_reservation()
            .context("Insufficient free space for metadata reservation.")?;

        Ok(())
    }

    pub fn volume_directory(&self) -> &Directory<ObjectStore> {
        self.volume_directory.get().unwrap()
    }

    pub fn set_volume_directory(&self, volume_directory: Directory<ObjectStore>) {
        self.volume_directory.set(volume_directory).unwrap();
    }

    pub fn add_store(&self, store: Arc<ObjectStore>) {
        if let Some(on_new_store) = &self.on_new_store {
            on_new_store(&store);
        }
        let mut inner = self.inner.write();
        let store_object_id = store.store_object_id();
        assert_ne!(store_object_id, inner.root_parent_store_object_id);
        assert_ne!(store_object_id, inner.root_store_object_id);
        assert_ne!(store_object_id, inner.allocator_object_id);
        inner.stores.insert(store_object_id, store);
    }

    pub fn forget_store(&self, store_object_id: u64) {
        let mut inner = self.inner.write();
        assert_ne!(store_object_id, inner.allocator_object_id);
        inner.stores.remove(&store_object_id);
        inner.reservations.remove(&store_object_id);
    }

    pub fn set_allocator(&self, allocator: Arc<Allocator>) {
        let mut inner = self.inner.write();
        assert!(!inner.stores.contains_key(&allocator.object_id()));
        inner.allocator_object_id = allocator.object_id();
        inner.allocator = Some(allocator);
    }

    pub fn allocator(&self) -> Arc<Allocator> {
        self.inner.read().allocator.clone().unwrap()
    }

    /// Applies `mutation` to `object` with `context`.
    pub fn apply_mutation(
        &self,
        object_id: u64,
        mutation: Mutation,
        context: &ApplyContext<'_, '_>,
        associated_object: AssocObj<'_>,
    ) -> Result<(), Error> {
        debug!(oid = object_id, mutation:?; "applying mutation");
        let object = {
            let mut inner = self.inner.write();
            match mutation {
                Mutation::BeginFlush => {
                    if let Some(entry) = inner.journal_checkpoints.get_mut(&object_id) {
                        match entry {
                            Checkpoints::Current(x) | Checkpoints::Both(x, _) => {
                                *entry = Checkpoints::Old(x.clone());
                            }
                            _ => {}
                        }
                    }
                }
                Mutation::EndFlush => {
                    if let Entry::Occupied(mut o) = inner.journal_checkpoints.entry(object_id) {
                        let entry = o.get_mut();
                        match entry {
                            Checkpoints::Old(_) => {
                                o.remove();
                            }
                            Checkpoints::Both(_, x) => {
                                *entry = Checkpoints::Current(x.clone());
                            }
                            _ => {}
                        }
                    }
                }
                Mutation::DeleteVolume => {
                    if let Some(store) = inner.stores.remove(&object_id) {
                        store.mark_deleted();
                    }
                    inner.reservations.remove(&object_id);
                    inner.journal_checkpoints.remove(&object_id);
                    return Ok(());
                }
                _ => {
                    if inner.dirties(object_id, &mutation) {
                        inner
                            .journal_checkpoints
                            .entry(object_id)
                            .and_modify(|entry| {
                                if let Checkpoints::Old(x) = entry {
                                    *entry =
                                        Checkpoints::Both(x.clone(), context.checkpoint.clone());
                                }
                            })
                            .or_insert_with(|| Checkpoints::Current(context.checkpoint.clone()));
                    }
                }
            }
            inner.journaling_object(object_id).unwrap()
        };
        associated_object.map(|o| o.will_apply_mutation(&mutation, object_id, self));
        object.apply_mutation(mutation, context, associated_object)
    }

    /// Replays `mutations` for a single transaction.  `journal_offsets` contains the per-object
    /// starting offsets; if the current transaction offset precedes an offset, the mutations for
    /// that object are ignored.  `context` contains the location in the journal file for this
    /// transaction and `end_offset` is the ending journal offset for this transaction.
    pub async fn replay_mutations(
        &self,
        mutations: Vec<(u64, Mutation)>,
        journal_offsets: &HashMap<u64, u64>,
        context: &ApplyContext<'_, '_>,
        end_offset: u64,
    ) -> Result<(), Error> {
        debug!(checkpoint = context.checkpoint.file_offset; "REPLAY");
        let txn_size = {
            let mut inner = self.inner.write();
            if end_offset > inner.last_end_offset {
                Some(end_offset - std::mem::replace(&mut inner.last_end_offset, end_offset))
            } else {
                None
            }
        };

        let allocator_object_id = self.inner.read().allocator_object_id;

        for (object_id, mutation) in mutations {
            if let Mutation::UpdateBorrowed { borrowed, from, paid } = mutation {
                // cladefs: as the transaction borrowed live (`did_commit_transaction`): what its
                // reservation didn't pay of its usage from `from`, which replay can't tell from
                // where the last it replayed ended, as a transaction paying for padding writes
                // nothing to the journal (TASK-218).
                if txn_size.is_some() {
                    let usage = end_offset.checked_sub(from).ok_or(FxfsError::Inconsistent)?;
                    let space = reserved_space_from_journal_usage(usage);
                    self.inner.write().borrowed_metadata_space = borrowed
                        .checked_add(space - space.min(paid))
                        .ok_or(FxfsError::Inconsistent)?;
                }
                continue;
            }

            // Don't replay mutations if the object doesn't want it.
            if let Some(&offset) = journal_offsets.get(&object_id) {
                if context.checkpoint.file_offset < offset {
                    continue;
                }
            }

            // If this is the first time we've encountered this store, we'll need to open it.
            if object_id != allocator_object_id {
                self.open_store(&self.root_store(), object_id).await?;
            }

            self.apply_mutation(object_id, mutation, context, AssocObj::None)?;
        }
        Ok(())
    }

    /// cladefs: opens the volume `object_id`, if it stayed closed (`Options::lazy_volumes`), while
    /// transactions commit: no commit finds what it requires changed. A closed volume wasn't
    /// counted in the metadata reservation, which pays for compacting the largest, so what it
    /// adds is reserved now, or it stays closed. Its graveyard is reaped as mounting does.
    pub async fn open_volume(&self, object_id: u64) -> Result<(), Error> {
        let root_store = self.root_store();
        let filesystem = root_store.filesystem();
        let read_only = filesystem.options().read_only;
        {
            let _commits = filesystem.lock_commits().await;
            if self.store(object_id).is_some() {
                return Ok(());
            }
            let required = self.required_reservation();
            self.open_store(&root_store, object_id).await?;
            let more = self.required_reservation() - required;
            if more > 0 && !read_only {
                let Some(reservation) = filesystem.allocator().reserve(None, more) else {
                    self.forget_store(object_id);
                    bail!(FxfsError::NoSpace);
                };
                reservation.move_to(self.metadata_reservation(), more);
            }
        }
        if !read_only {
            let store = self.store(object_id).ok_or(FxfsError::NotFound)?;
            filesystem.graveyard().initial_reap(&store).await?;
        }
        Ok(())
    }

    /// cladefs: closes the volume `object_id` again (`Options::lazy_volumes`), once nothing has it
    /// open: what it holds in memory goes, its layers stay, and `open_volume` opens it again
    /// (backlog doc-10, an idle repo's mount expiring, TASK-136.6). What the journal holds of it
    /// is flushed into its layers first, which then hold all it has; on a read-only filesystem,
    /// which can't flush, it stays open, as its layers lack what replay applied. A replay opens it
    /// again while the journal still names it, as it opens any volume written to lately.
    /// `Unavailable` if it is in use: anything else holds its store, or it was written to as it
    /// closed.
    pub async fn close_volume(&self, object_id: u64) -> Result<(), Error> {
        let root_store = self.root_store();
        let filesystem = root_store.filesystem();
        ensure!(filesystem.options().lazy_volumes, FxfsError::InvalidArgs);
        ensure!(!self.is_system_store(object_id), FxfsError::InvalidArgs);
        if self.store(object_id).is_none() {
            return Ok(());
        }
        // What it queued to reap, done: nothing reaps in a volume closed under it. A read-only
        // filesystem's graveyard reaps nothing.
        let read_only = filesystem.options().read_only;
        if !read_only {
            filesystem.graveyard().flush().await;
        }
        let keys = lock_keys![LockKey::flush(object_id)];
        let _flushing = filesystem.lock_manager().write_lock(keys).await;
        if self.needs_flush(object_id) {
            ensure!(!read_only, FxfsError::ReadOnlyFilesystem);
            let store = self.store(object_id).ok_or(FxfsError::NotFound)?;
            store.flush_guarded_with_reason(FlushReason::Journal(ForceMajor::False)).await?;
        }
        let _commits = filesystem.lock_commits().await;
        ensure!(!self.needs_flush(object_id), FxfsError::Unavailable);
        let mut inner = self.inner.write();
        let Some(store) = inner.stores.get(&object_id) else { return Ok(()) };
        ensure!(Arc::strong_count(store) == 1, FxfsError::Unavailable);
        inner.stores.remove(&object_id);
        inner.reservations.remove(&object_id);
        Ok(())
    }

    /// cladefs: how many volumes are open: not the root store's (`close_volume`).
    pub fn open_volumes(&self) -> usize {
        let inner = self.inner.read();
        let system = [inner.root_store_object_id, inner.root_parent_store_object_id];
        inner.stores.keys().filter(|id| !system.contains(id)).count()
    }

    /// Opens the specified store if it isn't already.  This is *not* thread-safe.
    async fn open_store(&self, parent: &Arc<ObjectStore>, object_id: u64) -> Result<(), Error> {
        if self.inner.read().stores.contains_key(&object_id) {
            return Ok(());
        }
        let cache = parent.filesystem().new_tree_cache();
        let store = ObjectStore::open(parent, object_id, Some(Box::new(cache)))
            .await
            .with_context(|| format!("Failed to open store {object_id}"))?;
        if let Some(on_new_store) = &self.on_new_store {
            on_new_store(&store);
        }
        assert!(self.inner.write().stores.insert(object_id, store).is_none());
        Ok(())
    }

    /// cladefs: reserves with the allocator what flushing the objects `transaction` dirties first
    /// adds to the metadata reservation (`FLUSH_COST_BLOCKS`), before it is written and applied,
    /// with nothing else committing meanwhile (the journal's writer lock); what it has no room for,
    /// the transaction borrows (`apply_transaction`), whatever its reservation (board 14243). It
    /// never fails one: a writeback of dirty data its reservation pays for is written however
    /// full the filesystem, as the guest drops what fails with ENOSPC, and refused, the first
    /// writeback to a store a compaction left clean failed (hotbed review 171).
    pub fn reserve_flush_charge(&self, transaction: &Transaction<'_>) -> FlushCharge {
        let amount = self.inner.read().flush_charge(transaction.mutations());
        let reserved = (amount > 0).then(|| self.allocator().reserve(None, amount)).flatten();
        FlushCharge { amount, reserved }
    }

    /// Called by the journaling system to apply a transaction.  `checkpoint` indicates the location
    /// in the journal file for this transaction.  Returns an optional mutation to be written to be
    /// included with the transaction. cladefs: `flush_charge` is what `reserve_flush_charge`
    /// reserved for it, which goes into the metadata reservation as its objects' charges do into
    /// what that requires.
    pub fn apply_transaction(
        &self,
        transaction: &mut Transaction<'_>,
        checkpoint: &JournalCheckpoint,
        flush_charge: FlushCharge,
    ) -> Result<Option<Mutation>, Error> {
        // Record old values so we can see what changes as a result of this transaction.
        let old_amount = self.metadata_reservation().amount();
        let old_required = self.inner.read().required_reservation();
        debug_assert_eq!(
            self.inner.read().flush_charge(transaction.mutations()),
            flush_charge.amount,
            "the transaction's flush charge changed after it was reserved"
        );

        debug!(checkpoint = checkpoint.file_offset; "BEGIN TXN");
        let mutations = transaction.take_mutations();
        let context =
            ApplyContext { mode: ApplyMode::Live(transaction), checkpoint: checkpoint.clone() };
        for TxnMutation { object_id, mutation, associated_object, .. } in mutations {
            self.apply_mutation(object_id, mutation, &context, associated_object)?;
        }
        debug!("END TXN");

        // cladefs: what the transaction's new objects to flush require, the allocator reserved: a
        // transaction that may borrow borrows the rest, below, as the requirement grew by it.
        let FlushCharge { amount: charge, reserved } = flush_charge;
        let paid = reserved.map_or(0, |reserved| {
            let amount = reserved.amount();
            reserved.move_to(self.metadata_reservation(), amount);
            amount
        });

        let borrowing = matches!(
            transaction.metadata_reservation,
            MetadataReservation::Borrowed | MetadataReservation::Borrowing(_)
        );
        Ok(if borrowing {
            // If this transaction is borrowing metadata, figure out what has changed and return a
            // mutation with the updated value for borrowed.  The transaction might have allocated
            // or deallocated some data from the metadata reservation, or it might have made a
            // change that means we need to reserve more or less space (e.g. we compacted).
            let new_amount = self.metadata_reservation().amount();
            let mut inner = self.inner.write();
            let new_required = inner.required_reservation();
            let add = old_amount + new_required;
            let sub = new_amount + old_required;
            if add >= sub {
                inner.borrowed_metadata_space += add - sub;
            } else {
                inner.borrowed_metadata_space =
                    inner.borrowed_metadata_space.saturating_sub(sub - add);
            }
            // cladefs: with what replay needs to find what the transaction borrows of its own usage,
            // which `did_commit_transaction` adds: where it counts it from, and what of it the
            // transaction's reservation pays, which is untouched until then.
            let paid = match &transaction.metadata_reservation {
                MetadataReservation::Borrowing(reservation) => reservation.amount(),
                _ => 0,
            };
            Some(Mutation::UpdateBorrowed {
                borrowed: inner.borrowed_metadata_space,
                from: inner.last_end_offset,
                paid,
            })
        } else {
            // This transaction should have had no impact on the metadata reservation or the amount
            // we need to reserve. cladefs: but for flushing the objects it dirtied first, which it
            // paid as far as the allocator had room (`reserve_flush_charge`).
            debug_assert_eq!(self.metadata_reservation().amount(), old_amount + paid);
            debug_assert_eq!(self.inner.read().required_reservation(), old_required + charge);
            (paid < charge).then(|| {
                // cladefs: the rest it borrows, which compacting repays, recorded as a borrowing
                // transaction's is, for replay to borrow it too: its own reservation pays for its
                // journal usage (`did_commit_transaction`), all of what it holds.
                let mut inner = self.inner.write();
                inner.borrowed_metadata_space += charge - paid;
                let paid = match &transaction.metadata_reservation {
                    MetadataReservation::Reservation(reservation) => reservation.amount(),
                    MetadataReservation::Hold(hold_amount) => *hold_amount,
                    _ => 0,
                };
                Mutation::UpdateBorrowed {
                    borrowed: inner.borrowed_metadata_space,
                    from: inner.last_end_offset,
                    paid,
                }
            })
        })
    }

    /// Called by the journaling system after a transaction has been written providing the end
    /// offset for the transaction so that we can adjust borrowed metadata space accordingly.
    pub fn did_commit_transaction(
        &self,
        transaction: &mut Transaction<'_>,
        _checkpoint: &JournalCheckpoint,
        end_offset: u64,
    ) {
        let reservation = self.metadata_reservation();
        // Before the lock, as `init_metadata_reservation` reads it.
        let journal_held = self.root_parent_store().filesystem().journal().journal_bytes();
        let mut inner = self.inner.write();
        let journal_usage = end_offset - std::mem::replace(&mut inner.last_end_offset, end_offset);
        inner.last_journal_usage = journal_usage;

        let txn_space = reserved_space_from_journal_usage(journal_usage);
        match &mut transaction.metadata_reservation {
            MetadataReservation::None => unreachable!(),
            MetadataReservation::Borrowed | MetadataReservation::Borrowing(_) => {
                // Account for the amount we need to borrow for the transaction itself now that we
                // know the transaction size: cladefs, what its reservation doesn't pay for, if it has
                // one.
                let paid = match &transaction.metadata_reservation {
                    MetadataReservation::Borrowing(txn_reservation) => {
                        let paid = txn_space.min(txn_reservation.amount());
                        txn_reservation.move_to(reservation, paid);
                        paid
                    }
                    _ => 0,
                };
                inner.borrowed_metadata_space += txn_space - paid;

                // This transaction borrowed metadata space, but it might have returned space to the
                // transaction that we can now give back to the allocator.
                let to_give_back = (reservation.amount() + inner.borrowed_metadata_space)
                    .saturating_sub(inner.required_reservation());
                if to_give_back > 0 {
                    reservation.give_back(to_give_back);
                }
            }
            MetadataReservation::Hold(hold_amount) => {
                // Transfer reserved space into the metadata reservation. cladefs: as for
                // `MetadataReservation::Reservation` below, a release build borrows what the hold
                // doesn't cover rather than panic; it commits only what the hold has, which its
                // reservation counts as reserved (TASK-60).
                let txn_reservation = transaction.allocator_reservation.unwrap();
                assert_ne!(
                    txn_reservation as *const _, reservation as *const _,
                    "MetadataReservation::Borrowed should be used."
                );
                let paid = txn_space.min(*hold_amount);
                if paid < txn_space && cfg!(debug_assertions) {
                    panic!("Transaction was larger than metadata reservation");
                }
                txn_reservation.commit(paid);
                if txn_reservation.owner_object_id() != reservation.owner_object_id() {
                    assert_eq!(
                        reservation.owner_object_id(),
                        None,
                        "Should not be mixing attributed owners."
                    );
                    inner
                        .allocator
                        .as_ref()
                        .unwrap()
                        .disown_reservation(txn_reservation.owner_object_id(), paid);
                }
                *hold_amount -= paid;
                reservation.add(paid);
                if paid < txn_space {
                    warn!(txn_space, paid; "A transaction used more metadata space than it held");
                    inner.borrowed_metadata_space += txn_space - paid;
                }
            }
            MetadataReservation::Reservation(txn_reservation) => {
                // Transfer reserved space into the metadata reservation. cladefs: a transaction larger
                // than it reserved for is a bug, which debug builds panic on, to find it; a release
                // build borrows what it doesn't hold, which compacting the journal repays, rather
                // than take the guest's kernel down with it (TASK-58, TASK-60).
                let paid = txn_space.min(txn_reservation.amount());
                if paid < txn_space && cfg!(debug_assertions) {
                    panic!(
                        "Insufficient reservation space: {paid} for {journal_usage} bytes of \
                         journal, {} mutations",
                        transaction.mutations().len()
                    );
                }
                txn_reservation.move_to(reservation, paid);
                if paid < txn_space {
                    warn!(txn_space, paid; "A transaction used more metadata space than it reserved");
                    inner.borrowed_metadata_space += txn_space - paid;
                }
            }
        }
        // cladefs: and the reservation follows the journal's blocks (`journal_held`), which the
        // reserved space is room for: what they took it gives back, what they freed it takes
        // again, as far as the allocator has room for it, and borrows the rest (board 14491).
        Self::follow_journal(&mut inner, reservation, journal_held);
        // Check that our invariant holds true.
        debug_assert_eq!(
            reservation.amount() + inner.borrowed_metadata_space,
            inner.required_reservation(),
            "txn_space: {}, reservation_amount: {}, borrowed: {}, required: {}",
            txn_space,
            reservation.amount(),
            inner.borrowed_metadata_space,
            inner.required_reservation(),
        );
    }

    /// cladefs: sets what the journal's blocks take, `journal_held`, and brings `reservation`, the
    /// metadata reservation, to what is then required of it: the excess given back to the
    /// allocator, which the journal's blocks took (its system bytes); what is short, as the
    /// journal freed blocks, reserved again, or borrowed if the allocator has no room for it.
    fn follow_journal(inner: &mut Inner, reservation: &Reservation, journal_held: u64) {
        inner.journal_held = journal_held;
        let (held, required) =
            (reservation.amount() + inner.borrowed_metadata_space, inner.required_reservation());
        if held > required {
            let given = (held - required).min(reservation.amount());
            reservation.give_back(given);
            inner.borrowed_metadata_space -= held - required - given;
        } else if held < required {
            let short = required - held;
            match inner.allocator.as_ref().unwrap().clone().reserve(None, short) {
                Some(more) => more.move_to(reservation, short),
                None => inner.borrowed_metadata_space += short,
            }
        }
    }

    /// Drops a transaction.  This is called automatically when a transaction is dropped.  If the
    /// transaction has been committed, it should contain no mutations and so nothing will get rolled
    /// back.  For each mutation, drop_mutation is called to allow for roll back (e.g. the allocator
    /// will unreserve allocations).
    pub fn drop_transaction(&self, transaction: &mut Transaction<'_>) {
        for TxnMutation { object_id, mutation, .. } in transaction.take_mutations() {
            self.journaling_object(object_id).map(|o| o.drop_mutation(mutation, transaction));
        }
    }

    /// Returns the journal file offsets that each object depends on and the checkpoint for the
    /// minimum offset.
    pub fn journal_file_offsets(&self) -> (HashMap<u64, u64>, Option<JournalCheckpoint>) {
        let inner = self.inner.read();
        let mut min_checkpoint = None;
        let mut offsets = HashMap::default();
        for (&object_id, checkpoint) in &inner.journal_checkpoints {
            let checkpoint = checkpoint.earliest();
            match &mut min_checkpoint {
                None => min_checkpoint = Some(checkpoint),
                Some(min_checkpoint) => {
                    if checkpoint.file_offset < min_checkpoint.file_offset {
                        *min_checkpoint = checkpoint;
                    }
                }
            }
            offsets.insert(object_id, checkpoint.file_offset);
        }
        (offsets, min_checkpoint.cloned())
    }

    /// Returns the checkpoint into the journal that the object depends on, or None if the object
    /// has no journaled updates.
    pub fn journal_checkpoint(&self, object_id: u64) -> Option<JournalCheckpoint> {
        self.inner
            .read()
            .journal_checkpoints
            .get(&object_id)
            .map(|checkpoints| checkpoints.earliest().clone())
    }

    /// Returns true if the object identified by `object_id` is known to have updates recorded in
    /// the journal that the object depends upon.
    pub fn needs_flush(&self, object_id: u64) -> bool {
        self.inner.read().journal_checkpoints.contains_key(&object_id)
    }

    /// Flushes changes to device with the given reason.
    /// Also returns the earliest known version of a struct on the filesystem.
    pub async fn flush(&self, reason: FlushReason) -> Result<Version, Error> {
        let objects = {
            let inner = self.inner.read();
            let mut object_ids = inner.journal_checkpoints.keys().cloned().collect::<Vec<_>>();
            // Process objects in reverse sorted order because that will mean we compact the root
            // object store last which will ensure we include the metadata from the compactions of
            // other objects.
            object_ids.sort_unstable();
            object_ids
                .iter()
                .rev()
                .filter_map(|oid| inner.journaling_object(*oid).map(|obj| (*oid, obj)))
                .collect::<Vec<_>>()
        };

        // As we iterate, keep track of the earliest version used by structs in these objects
        let mut earliest_version: Version = LATEST_VERSION;
        for (object_id, object) in objects {
            // cladefs: box each object's flush in a non-inlined frame so journal compaction
            // stays within 16 KiB guest kernel stacks without extra task yields (TASK-26.9).
            let object_earliest_version = crate::boxed(|| object.flush(reason))
                .await
                .with_context(|| format!("Failed to flush oid {object_id}"))?;
            if object_earliest_version < earliest_version {
                earliest_version = object_earliest_version;
            }
        }

        Ok(earliest_version)
    }

    pub fn journaling_object(&self, object_id: u64) -> Option<Arc<dyn JournalingObject>> {
        self.inner.read().journaling_object(object_id)
    }

    pub fn init_metadata_reservation(&self) -> Result<(), Error> {
        if self.root_parent_store().filesystem().options().read_only {
            // We don't need metadata reservations in read-only mode.
            return Ok(());
        }
        // Before the lock: the journal asks nothing of the object manager, but its filesystem is
        // reached through a store, which is.
        let journal_bytes = self.root_parent_store().filesystem().journal().journal_bytes();
        let mut inner = self.inner.write();
        inner.journal_held = journal_bytes;
        let required = inner.required_reservation();
        ensure!(required >= inner.borrowed_metadata_space, FxfsError::Inconsistent);
        let allocator = inner.allocator.as_ref().cloned().unwrap();
        let wanted = required - inner.borrowed_metadata_space;
        // cladefs: what the objects the journal holds updates of take to flush (board 14243) is
        // borrowed if the filesystem has no room for it: a journal an earlier build wrote, which
        // charged nothing for it, may have filled it. Refused, its mount would fail where that
        // build's didn't; borrowed, compacting it is no worse off than that build's (hotbed
        // question 586).
        let reservation = match allocator.clone().reserve(None, wanted) {
            Some(reservation) => reservation,
            None => {
                let free = (Saturating(allocator.get_disk_bytes()) - allocator.get_used_bytes()).0;
                let charges = inner.flush_charges();
                let borrowed = wanted.saturating_sub(free);
                let reservation = (borrowed <= charges)
                    .then(|| allocator.clone().reserve(None, wanted - borrowed))
                    .flatten()
                    .with_context(|| {
                        let parts = (REQUIRED_PARTS.iter().zip(inner.required_parts()))
                            .map(|(name, bytes)| format!("{name} {bytes}"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!(
                            "Failed to reserve {} ({parts}) - {} = {} bytes ({charges} of them \
                             borrowable), free={free}, owner_bytes={}",
                            required,
                            inner.borrowed_metadata_space,
                            wanted,
                            allocator.owner_bytes_debug(),
                        )
                    })?;
                warn!(borrowed; "No room for the whole metadata reservation: borrowed");
                inner.borrowed_metadata_space += borrowed;
                reservation
            }
        };
        self.metadata_reservation.set(reservation).unwrap();
        Ok(())
    }

    pub fn metadata_reservation(&self) -> &Reservation {
        self.metadata_reservation.get().unwrap()
    }

    pub fn is_metadata_reservation(&self, reservation: &Reservation) -> bool {
        self.metadata_reservation.get().is_some_and(|r| std::ptr::eq(r, reservation))
    }

    pub fn update_reservation(&self, object_id: u64, amount: u64) {
        self.inner.write().reservations.insert(object_id, amount);
    }

    pub fn reservation(&self, object_id: u64) -> Option<u64> {
        self.inner.read().reservations.get(&object_id).cloned()
    }

    /// Returns the maximum reservation held by any object (the amount budgeted for compaction).
    pub fn max_store_reservation(&self) -> u64 {
        self.inner.read().reservations.values().max().cloned().unwrap_or(0)
    }

    /// cladefs: what the metadata reservation requires for flushing the objects the journal holds
    /// updates of, whatever they hold (`FLUSH_COST_BLOCKS`, board 14243).
    pub fn flush_charges(&self) -> u64 {
        self.inner.read().flush_charges()
    }

    pub fn set_reserved_space(&self, amount: u64) {
        self.inner.write().reserved_space = amount;
    }

    pub fn last_end_offset(&self) -> u64 {
        self.inner.read().last_end_offset
    }

    /// cladefs: what the last transaction applied took of the journal, as its reservation was
    /// charged (`did_commit_transaction`).
    pub fn last_journal_usage(&self) -> u64 {
        self.inner.read().last_journal_usage
    }

    pub fn set_last_end_offset(&self, v: u64) {
        self.inner.write().last_end_offset = v;
    }

    pub fn borrowed_metadata_space(&self) -> u64 {
        self.inner.read().borrowed_metadata_space
    }

    /// cladefs: the metadata space borrowed as replay found it, before the mount's own transactions:
    /// what the filesystem had borrowed when it stopped, unless a crash lost some of what it did.
    pub fn borrowed_as_replayed(&self) -> u64 {
        self.inner.read().borrowed_as_replayed
    }

    pub fn set_borrowed_metadata_space(&self, v: u64) {
        self.inner.write().borrowed_metadata_space = v;
    }

    /// The stores the filesystem has open.
    pub fn stores(&self) -> Vec<Arc<ObjectStore>> {
        self.inner.read().stores.values().cloned().collect()
    }

    /// Normally, we make new transactions pay for overheads incurred by the journal, such as
    /// checksums and padding, but if the journal has discarded a significant amount after a replay,
    /// we run the risk of there not being enough reserved.  To handle this, if the amount is
    /// significant, we force the journal to borrow the space (using a journal created transaction).
    pub fn needs_borrow_for_journal(&self, checkpoint: u64) -> bool {
        self.unaccounted_journal(checkpoint) > 256
    }

    /// cladefs: how much of the journal, up to `checkpoint`, no transaction has paid for yet: what
    /// the next must.
    pub fn unaccounted_journal(&self, checkpoint: u64) -> u64 {
        checkpoint.checked_sub(self.inner.read().last_end_offset).unwrap()
    }
}

/// cladefs: what flushing the objects a transaction dirties first adds to the metadata reservation
/// (`FLUSH_COST_BLOCKS`), and what of that the allocator reserved before the transaction applied
/// (`ObjectManager::reserve_flush_charge`): the rest it borrows (board 14243).
pub struct FlushCharge {
    amount: u64,
    reserved: Option<Reservation>,
}

/// ReservationUpdate is an associated object that sets the amount reserved for an object
/// (overwriting any previous amount). Updates must be applied as part of a transaction before
/// did_commit_transaction runs because it will reconcile the accounting for reserved metadata
/// space.
pub struct ReservationUpdate(u64);

impl ReservationUpdate {
    pub fn new(amount: u64) -> Self {
        Self(amount)
    }
}

impl AssociatedObject for ReservationUpdate {
    fn will_apply_mutation(&self, _mutation: &Mutation, object_id: u64, manager: &ObjectManager) {
        manager.update_reservation(object_id, self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::ObjectManager;
    use crate::filesystem::{FxFilesystem, FxFilesystemBuilder};
    use crate::fsck::fsck;
    use crate::object_store::directory::Directory;
    use crate::object_store::transaction::{Options, lock_keys};
    use crate::object_store::volume::root_volume;
    use crate::object_store::{LockKey, NewChildStoreOptions, ObjectStore};
    use std::sync::Arc;
    use storage_device::DeviceHolder;
    use storage_device::fake_device::FakeDevice;

    /// As many volumes as the guest's snapshot tests format on a cell, and more.
    const STORES: usize = 300;

    /// The invariant of the metadata reservation: what it holds and what was borrowed of it is what
    /// it requires.
    fn assert_balanced(fs: &FxFilesystem, when: &str) {
        let objects = fs.object_manager();
        let (amount, borrowed) =
            (objects.metadata_reservation().amount(), objects.borrowed_metadata_space());
        assert_eq!(
            amount + borrowed,
            objects.required_reservation(),
            "{when}: reservation {amount} + borrowed {borrowed}"
        );
    }

    /// Creates the file `name` in `store`'s root directory: a few hundred bytes of the journal.
    async fn create_file(store: &Arc<ObjectStore>, name: &str) {
        let dir =
            Directory::open(store, store.root_directory_object_id()).await.expect("open failed");
        let mut transaction = store
            .new_transaction(
                lock_keys![LockKey::object(store.store_object_id(), dir.object_id())],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        dir.create_child_file(&mut transaction, name).await.expect("create_child_file failed");
        transaction.commit().await.expect("commit failed");
    }

    /// cladefs: a nearly full filesystem whose journal grew while it was full, its compactions
    /// waiting, mounts after a crash. The journal's own blocks, which the allocator counts used,
    /// are what the metadata reservation's reserved space is room for: the filesystem running
    /// reserved that once, as it formatted, and a mount counted those blocks twice, refusing to
    /// mount (board 14491: fxfs-guest-script-killed-full's fsck, 50 KB short).
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_a_full_filesystem_with_a_grown_journal_mounts() {
        use crate::errors::FxfsError;
        use crate::filesystem::SyncOptions;
        use crate::object_handle::{ObjectHandle, WriteObjectHandle};
        use crate::object_store::journal::JournalOptions;
        // A journal as large as the guest's on its 64 MiB volume (guest/rust/cladefs-linux's
        // `Fs::open`), which grows to half a megabyte here, its compactions waiting.
        let journal = || JournalOptions { reclaim_size: 2 << 20, ..Default::default() };
        let device = DeviceHolder::new(FakeDevice::new(16384, 4096));
        let fs = FxFilesystemBuilder::new()
            .journal_options(journal())
            .format(true)
            .open(device)
            .await
            .expect("open failed");
        let store = root_volume(fs.clone())
            .await
            .expect("root_volume failed")
            .new_volume("vol", NewChildStoreOptions::default())
            .await
            .expect("new_volume failed");
        let dir =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");
        let full = |e: &anyhow::Error| {
            e.root_cause().downcast_ref::<FxfsError>() == Some(&FxfsError::NoSpace)
        };
        // Filled: a file appended to, in large pieces and then in blocks, until nothing fits.
        let mut transaction = store
            .new_transaction(
                lock_keys![LockKey::object(store.store_object_id(), dir.object_id())],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        let file = dir
            .create_child_file(&mut transaction, "fill")
            .await
            .expect("create_child_file failed");
        transaction.commit().await.expect("commit failed");
        let block_size = fs.block_size().get() as usize;
        for piece in [64 * block_size, block_size] {
            let mut buffer = file.allocate_buffer(piece).await;
            buffer.as_mut_slice().fill(0xf1);
            loop {
                match file.write_or_append(None, buffer.as_ref()).await {
                    Ok(_) => {}
                    Err(e) if full(&e) => break,
                    Err(e) => panic!("write failed: {e:?}"),
                }
            }
        }
        // The journal grown while full: files created, borrowing, each synced, which pads the
        // journal to its next block.
        fs.journal().pause_compactions().await;
        for i in 0..100 {
            let mut transaction = store
                .new_transaction(
                    lock_keys![LockKey::object(store.store_object_id(), dir.object_id())],
                    Options { borrow_metadata_space: true, ..Default::default() },
                )
                .await
                .expect("new_transaction failed");
            dir.create_child_file(&mut transaction, &format!("f{i}"))
                .await
                .expect("create_child_file failed");
            transaction.commit().await.expect("commit failed");
            fs.sync(SyncOptions { flush_device: true, ..Default::default() })
                .await
                .expect("sync failed");
        }
        let held = fs.journal().journal_bytes();
        assert!(held > 400_000, "the journal holds {held} bytes");
        let crashed = fs.device().snapshot().expect("snapshot failed");
        let mounted = FxFilesystemBuilder::new()
            .journal_options(journal())
            .open(crashed)
            .await
            .expect("mount failed");
        fsck(mounted.clone()).await.expect("fsck failed");
        mounted.close().await.expect("close failed");
        fs.journal().resume_compactions();
        std::mem::drop((file, dir, store));
        fs.close().await.expect("close failed");
    }

    /// cladefs: hundreds of stores, each dirtied by a few hundred bytes of the journal, compact all
    /// at once without running the metadata reservation out of room, though a store's flush writes
    /// a layer file of three blocks however little it holds (board 14243): new volumes, a file
    /// created in each, and another, replayed by a mount. Compactions wait meanwhile, so that one
    /// compaction flushes them all.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_hundreds_of_lightly_dirtied_stores_compact() {
        let device = DeviceHolder::new(FakeDevice::new(32768, 4096));
        let fs = FxFilesystemBuilder::new().format(true).open(device).await.expect("open failed");
        fs.journal().pause_compactions().await;
        let volumes = root_volume(fs.clone()).await.expect("root_volume failed");
        let mut stores = Vec::new();
        for volume in 0..STORES {
            let store = volumes
                .new_volume(&format!("v{volume}"), NewChildStoreOptions::default())
                .await
                .expect("new_volume failed");
            stores.push(store);
        }
        assert_balanced(&fs, "new volumes");
        fs.journal().force_compact().await.expect("compacting new volumes");
        assert_balanced(&fs, "new volumes compacted");

        for store in &stores {
            create_file(store, "file").await;
        }
        assert_balanced(&fs, "a file in each");
        fs.journal().force_compact().await.expect("compacting a file in each");
        assert_balanced(&fs, "a file in each compacted");

        for store in &stores {
            create_file(store, "another").await;
        }
        drop((stores, volumes));
        fs.close().await.expect("close failed");
        let device = fs.take_device().await;
        device.reopen(false);
        let fs = FxFilesystemBuilder::new().open(device).await.expect("open failed");
        assert_balanced(&fs, "another file in each, replayed");
        fs.journal().force_compact().await.expect("compacting another file in each, replayed");
        assert_balanced(&fs, "another file in each, replayed and compacted");
        fsck(fs.clone()).await.expect("fsck failed");
        fs.close().await.expect("close failed");
    }

    /// cladefs: the journal compacts on its own before the stores it dirtied cost more to flush
    /// than it keeps room for compacting (board 14243): hundreds of new volumes, each a few hundred
    /// bytes of the journal, well under what compacts it.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_many_dirty_stores_compact_the_journal() {
        let device = DeviceHolder::new(FakeDevice::new(32768, 4096));
        let fs = FxFilesystemBuilder::new().format(true).open(device).await.expect("open failed");
        let compactions =
            || fs.journal().stats().compactions.load(std::sync::atomic::Ordering::Relaxed);
        let before = compactions();
        let volumes = root_volume(fs.clone()).await.expect("root_volume failed");
        for volume in 0..STORES {
            volumes
                .new_volume(&format!("v{volume}"), NewChildStoreOptions::default())
                .await
                .expect("new_volume failed");
        }
        assert!(compactions() > before, "{} compactions", compactions() - before);
        fs.close().await.expect("close failed");
    }

    /// cladefs: `cached_layer_chunks` and `evict_layer_chunks` must not block while
    /// `ObjectManager::inner` is write-locked (as `add_store` holds it while inserting into
    /// `stores`; TASK-292, TASK-596, hotbed review 109).
    #[test]
    fn test_layer_chunk_shrinker_under_inner_write_lock() {
        let manager = ObjectManager::new(storage_units::BlockSize::SIZE_4KIB, None);
        let _write_guard = manager.inner.write();
        assert_eq!(manager.cached_layer_chunks(), 0);
        assert_eq!(manager.evict_layer_chunks(16), 0);
    }
}
