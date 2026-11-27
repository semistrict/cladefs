// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Fuchsia's `src/storage/fxfs/src/object_store/journal.rs`. cladefs snapshots (volume snapshots that
// share their source's layers and extents) deviate from upstream here: replay validates Snapshot
// mutations. Data is located in extents of cladefs's extent store (backlog doc-4), not at device
// offsets: journal records hold extent ranges, and replay checks extents' offsets, not a device
// size. Flushing the device frees the extents that died before it (`did_flush_device`), in the
// background, which the next flush waits for, and which can fail; when some of them need it, the
// journal first records the flush, and flushes again (`record_device_flush`). Extents are written
// once: the journal is no longer an object in `root_parent` or the allocator — its 4 KiB blocks
// live at position-derived extent IDs in contiguous `JournalSpan`s tracked by `Root` and extended
// in-stream by `JournalRecord::JournalSpan`, with a single atomic super-block in `Root` (boards
// 6923, 6926). On writable mount or snapshot thaw onto a new cell, the journal rotates to a fresh
// span in `Root` without probing or rewriting the super-block, and superseded journal blocks are
// freed directly after `commit_root`. A sync's barrier records how far the journal was written in the
// store's root it sets (`Root::journal_end`), and a replay that ends before there, at damage, is
// refused (TASK-153.13). So is a journal that starts within a block that doesn't verify
// (TASK-149.23). How far the journal is written, flushed and synced is one value
// (`JournalPosition`, position.rs, TASK-160.25).

//! The journal is an ever-extending stream of variable-length records describing mutations to be
//! applied to objects, stored in 4 KiB blocks addressed by position-derived extent IDs
//! (`JournalSpan`). Each block carries a trailing fletcher64 checksum seeded with the previous
//! block's checksum. Superseded blocks are freed directly after a super-block commits via
//! `commit_root`.

mod bootstrap_handle;
mod checksum_list;
mod position;
mod reader;
pub mod super_block;
mod writer;

use crate::checksum::{Checksum, Checksums};
use crate::errors::FxfsError;
use crate::filesystem::{
    ApplyContext, ApplyMode, FlushReason, ForceMajor, FxFilesystem, SyncOptions,
};
use crate::hooks::HooksHandle;
use crate::log::*;
use crate::object_handle::{ObjectHandle as _, ReadObjectHandle};
use crate::object_store::allocator::Allocator;
use crate::object_store::graveyard::Graveyard;
use crate::object_store::journal::bootstrap_handle::{
    JournalExtentHandle, extents_in_spans, prune_spans_before, truncate_spans_at,
};
use crate::object_store::journal::checksum_list::ChecksumList;
use crate::object_store::journal::position::JournalPosition;
use crate::object_store::journal::reader::{JournalReader, ReadResult};
use crate::object_store::journal::super_block::{
    JournalSpan, Root, SuperBlockHeader, SuperBlockManager,
};
use crate::object_store::journal::writer::JournalWriter;
use crate::object_store::object_manager::{ObjectManager, reserved_space_from_journal_usage};
use crate::object_store::transaction::{
    AllocatorMutation, Mutation, MutationV66, ObjectMutationIterator, ObjectStoreMutation, Options,
    TRANSACTION_MAX_JOURNAL_USAGE, Transaction, lock_keys,
};
use crate::object_store::{
    AssocObj, DataObjectHandle, HandleOwner, INVALID_OBJECT_ID, NewChildStoreOptions, ObjectStore,
};
use crate::serialized_types::{EARLIEST_SUPPORTED_VERSION, LATEST_VERSION, Version, Versioned};
use anyhow::{Context, Error, anyhow, bail, ensure};
use core::iter::Iterator;
use event_listener::Event;
use fprint::TypeFingerprint;
use fuchsia_sync::Mutex;
use futures::FutureExt as _;
use futures::future::poll_fn;
use rustc_hash::FxHashMap as HashMap;
use serde::{Deserialize, Serialize};
use static_assertions::const_assert;
use std::clone::Clone;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Poll, Waker};
use storage_device::{ExtentId, ExtentRange};
use storage_units::BlockSize;

// The journal stream is written in blocks of this size, each a write-once extent of its own.
pub const BLOCK_SIZE: BlockSize = BlockSize::SIZE_4KIB;

// Default number of 4 KiB blocks reserved per position-derived journal span (128 KiB).
const DEFAULT_SPAN_BLOCKS: u64 = 32;
const CHUNK_SIZE: u64 = DEFAULT_SPAN_BLOCKS * 4096;
const MIN_SPAN_BLOCKS: u64 = 4;
const_assert!(CHUNK_SIZE > TRANSACTION_MAX_JOURNAL_USAGE);

// Trigger background compaction once this many spans are live in `Root`, keeping `Root` well
// under `MAX_ROOT_SPANS` across repeated remounts.
const COMPACT_SPAN_THRESHOLD: usize = 32;

// See the comment for the `reclaim_size` member of Inner.
pub const DEFAULT_RECLAIM_SIZE: u64 = 524_288;

// Temporary space that should be reserved for the journal.  For example: space that is currently
// used in the journal file but cannot be deallocated yet because we are flushing.
pub const RESERVED_SPACE: u64 = 1_048_576;

/// cladefs: how much room the metadata reservation keeps over what compacting the largest store
/// needs, below which the journal compacts however little of `reclaim_size` it holds (but at least
/// `MIN_COMPACTION_BYTES`). Transactions that borrow metadata space, such as unlinks, can drain
/// the reservation faster than the journal fills: each is charged its share of the journal four
/// times over (`reserved_space_from_journal_usage`), padding from the syncs before it included.
/// Upstream's small journal compacts long before; the guest's, of up to 8 MiB, did not (PostgreSQL's
/// initdb failed with ENOSPC). Nothing waits for the compaction: a transaction waiting for it may
/// hold what it needs, as a guest's DAX store holds the extent device's gate a commit waits for.
const METADATA_LOW_MARGIN: u64 = RESERVED_SPACE / 2;
const MIN_COMPACTION_BYTES: u64 = RESERVED_SPACE / 8;

// Whenever the journal is replayed (i.e. the system is unmounted and remounted), we reset the
// journal stream, at which point any half-complete transactions are discarded.  We indicate a
// journal reset by XORing the previous block's checksum with this mask, and using that value as a
// seed for the next journal block. cladefs: public for the test of corrupt images, which reseals what
// it flips (crates/fxfs-posix/src/corrupt_tests.rs).
pub const RESET_XOR: u64 = 0xffffffffffffffff;

// To keep track of offsets within a journal file, we need both the file offset and the check-sum of
// the preceding block, since the check-sum of the preceding block is an input to the check-sum of
// every block.
pub type JournalCheckpoint = JournalCheckpointV32;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TypeFingerprint)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub struct JournalCheckpointV32 {
    pub file_offset: u64,

    // Starting check-sum for block that contains file_offset i.e. the checksum for the previous
    // block.
    pub checksum: Checksum,

    // If versioned, the version of elements stored in the journal. e.g. JournalRecord version.
    // This can change across reset events so we store it along with the offset and checksum to
    // know which version to deserialize.
    pub version: Version,
}

pub type JournalRecord = JournalRecordV66;

#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Serialize, Deserialize, TypeFingerprint, Versioned)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub enum JournalRecordV66 {
    EndBlock,
    Mutation {
        object_id: u64,
        mutation: MutationV66,
    },
    /// Commits records in the transaction.
    Commit,
    /// Discard all mutations with offsets greater than or equal to the given offset.
    Discard(u64),
    /// Indicates the device was flushed at the given journal offset.
    DidFlushDevice(u64),
    /// Checksums for a data range written by this transaction.
    DataChecksums(ExtentRange, crate::checksum::ChecksumsV38, bool),
    /// cladefs: extends the journal stream with a new position-derived span (board 6923).
    /// Replay accepts it only when the handle's current last span matches
    /// `(prev_cell, prev_end_sequence)` and ends at `span.start_offset`.
    JournalSpan {
        span: JournalSpan,
        prev_cell: u64,
        prev_end_sequence: u64,
    },
}

/// cladefs: `JournalRecord::Mutation`, borrowing its mutation, which serializes as the record does:
/// its variants are the record's, in order, as far as `Mutation`.
#[derive(Serialize)]
enum JournalRecordRef<'a> {
    #[allow(dead_code)]
    EndBlock,
    Mutation {
        object_id: u64,
        mutation: &'a Mutation,
    },
}

/// cladefs: a transaction's records, serialized before the journal's writer's lock
/// (`Journal::serialize_records`).
#[derive(Default)]
struct SerializedRecords {
    bytes: Vec<u8>,
    /// Where each record ends in `bytes`.
    ends: Vec<usize>,
}

/// cladefs: the bytes of the journal `JournalRecord::Mutation { object_id, mutation }` takes.
pub(super) fn mutation_record_bytes(object_id: u64, mutation: &Mutation) -> u64 {
    use bincode::Options;
    let record = JournalRecordRef::Mutation { object_id, mutation };
    let options = bincode::DefaultOptions::new().allow_trailing_bytes();
    options.serialized_size(&record).unwrap() // Measuring has no limit to reach.
}

/// cladefs: the bytes of the journal `JournalRecord::DataChecksums` takes, of `checksums` of `range`.
pub(super) fn checksums_record_bytes(
    range: &ExtentRange,
    checksums: &[Checksum],
    first_write: bool,
) -> u64 {
    use bincode::Options;
    let sums = Checksums::fletcher(checksums.to_vec());
    let record = JournalRecord::DataChecksums(range.clone(), sums, first_write);
    let options = bincode::DefaultOptions::new().allow_trailing_bytes();
    options.serialized_size(&record).unwrap() // Measuring has no limit to reach.
}

impl SerializedRecords {
    /// Serializes `JournalRecord::Mutation { object_id, mutation }`, as `write_record` would.
    fn push(&mut self, object_id: u64, mutation: &Mutation) {
        use bincode::Options;
        let record = JournalRecordRef::Mutation { object_id, mutation };
        let options = bincode::DefaultOptions::new().allow_trailing_bytes();
        match JournalRecord::max_serialized_size() {
            Some(limit) => options.with_limit(limit).serialize_into(&mut self.bytes, &record),
            None => options.serialize_into(&mut self.bytes, &record),
        }
        .unwrap(); // Writing to a Vec cannot fail.
        self.ends.push(self.bytes.len());
    }

    fn records(&self) -> impl Iterator<Item = &[u8]> {
        let starts = std::iter::once(0).chain(self.ends.iter().copied());
        starts.zip(self.ends.iter().copied()).map(|(start, end)| &self.bytes[start..end])
    }
}

/// The journal records a stream of mutations that are to be applied to other objects.  At mount
/// time, these records can be replayed into memory.  It provides a way to quickly persist changes
/// without having to make a large number of writes; they can be deferred to a later time (e.g.
/// when a sufficient number have been queued).  It also provides support for transactions, the
/// ability to have mutations that are to be applied atomically together.
pub struct Journal {
    objects: Arc<ObjectManager>,
    hooks: Arc<HooksHandle>,
    handle: OnceLock<JournalExtentHandle>,
    super_block_manager: SuperBlockManager,
    inner: Mutex<Inner>,
    writer_mutex: Mutex<()>,
    sync_mutex: futures::lock::Mutex<()>,
    // cladefs: bytes per minted position-derived journal span (`JournalOptions::chunk_size`).
    chunk_size: u64,

    // This event is used when we are waiting for a compaction to free up journal space.
    reclaim_event: Event,

    // cladefs: what it waited for and did, as `stats` says.
    stats: JournalStats,

    // cladefs: block-aligned high-water mark of journal bytes written or buffered, used without
    // locking `inner` by `Allocator::needs_sync` to account for physical journal bytes on device.
    writer_end_block: AtomicU64,
}

/// cladefs: how often, and how long, transactions waited for the journal's space, which compactions
/// free, and how many compactions there were, and how long they took: what a guest's Fxfs waits on
/// that it can't otherwise tell from idle (TASK-91.7).
#[derive(Default)]
pub struct JournalStats {
    pub space_waits: AtomicU64,
    pub space_wait_ns: AtomicU64,
    pub compactions: AtomicU64,
    pub compaction_ns: AtomicU64,
}

struct Inner {
    super_block_header: SuperBlockHeader,

    // cladefs: how far the journal has got, as written, flushed and synced:
    // `JournalPosition` (TASK-160.25).
    position: JournalPosition,

    // The writer for the journal.
    writer: JournalWriter,

    // Set when a reset is encountered during a read.
    // Used at write pre_commit() time to ensure we write a version first thing after a reset.
    output_reset_version: bool,

    // Waker for the flush task.
    flush_waker: Option<Waker>,

    // Indicates the journal has been terminated.
    terminate: bool,

    // Latched error indicating reason for journal termination if not graceful.
    terminate_reason: Option<Error>,

    // Disable compactions.
    disable_compactions: bool,

    // When true, compactions are paused.
    compactions_paused: bool,

    // True if compactions are running.
    compaction_running: bool,

    // Waker for the sync task for when it's waiting for the flush task to finish.
    sync_waker: Option<Waker>,

    // In the steady state, the journal should fluctuate between being approximately half of this
    // number and this number.  New super-blocks will be written every time about half of this
    // amount is written to the journal.
    reclaim_size: u64,

    // If true and `needs_barrier`, issue a pre-barrier on the first device write of each journal
    // write (which happens in multiples of `BLOCK_SIZE`). This ensures that all the corresponding
    // data writes make it to disk before the journal gets written to.
    barriers_enabled: bool,

    // If true, indicates that data write requests have been made to the device since the last
    // journal write.
    needs_barrier: bool,

    // True if a compaction is being forced for reasons other than the journal being full.
    forced_compaction: bool,

    // cladefs: see `JournalOptions::lazy_flush`.
    lazy_flush: Option<LazyFlush>,

    // cladefs: whether a lazy flush task's timer runs for the bytes buffered, so that a transaction
    // needn't wake it, which costs scheduling it each time, unless it has more to do.
    lazy_flush_armed: bool,
}

impl Inner {
    fn terminate(&mut self, reason: Option<Error>) {
        self.terminate = true;

        if let Some(err) = reason {
            error!(error:? = err; "Terminating journal");
            // Log previous error if one was already set, otherwise latch the error.
            if let Some(prev_err) = self.terminate_reason.as_ref() {
                error!(error:? = prev_err; "Journal previously terminated");
            } else {
                self.terminate_reason = Some(err);
            }
        }

        if let Some(waker) = self.flush_waker.take() {
            waker.wake();
        }
        if let Some(waker) = self.sync_waker.take() {
            waker.wake();
        }
    }
}

pub struct JournalOptions {
    /// In the steady state, the journal should fluctuate between being approximately half of this
    /// number and this number.  New super-blocks will be written every time about half of this
    /// amount is written to the journal.
    pub reclaim_size: u64,

    // If true, issue a pre-barrier on the first device write of each journal write (which happens
    // in multiples of `BLOCK_SIZE`). This ensures that all the corresponding data writes make it
    // to disk before the journal gets written to.
    pub barriers_enabled: bool,

    /// cladefs: when the flush task writes the journal: as each block fills, as upstream, unless
    /// set.
    pub lazy_flush: Option<LazyFlush>,

    /// cladefs: how much the journal's file is extended by at a time: a test's sequence too short
    /// to write 128 KiB of it extends it all the same.
    pub chunk_size: u64,
}

/// cladefs: the flush task writes the journal once this much of it is buffered, or once the oldest
/// buffered block has waited `delay`, rather than as each block fills, so that metadata operations
/// whose transactions fit in memory issue no device requests (the guest kernel's Fxfs, TASK-26.6).
/// Whatever waits for the journal (syncs, super-blocks) has it written at once.
#[derive(Clone, Copy, Debug)]
pub struct LazyFlush {
    pub bytes: usize,
    pub delay: std::time::Duration,
}

impl Default for JournalOptions {
    fn default() -> Self {
        JournalOptions {
            reclaim_size: DEFAULT_RECLAIM_SIZE,
            chunk_size: CHUNK_SIZE,
            barriers_enabled: false,
            lazy_flush: None,
        }
    }
}

struct JournaledTransactions {
    transactions: Vec<JournaledTransaction>,
    device_flushed_offset: u64,
}

#[derive(Debug, Default)]
pub struct JournaledTransaction {
    pub checkpoint: JournalCheckpoint,
    pub root_parent_mutations: Vec<Mutation>,
    pub root_mutations: Vec<Mutation>,
    /// List of (store_object_id, mutation).
    pub non_root_mutations: Vec<(u64, Mutation)>,
    pub end_offset: u64,
    pub checksums: Vec<JournaledChecksums>,

    /// Records offset + 1 of the matching begin_flush transaction. The +1 is because we want to
    /// ignore the begin flush transaction; we don't need or want to replay it.
    pub end_flush: Option<(/* store_id: */ u64, /* begin offset: */ u64)>,

    /// The volume which was deleted in this transaction, if any.
    pub volume_deleted: Option</* store_id: */ u64>,
}

impl JournaledTransaction {
    fn new(checkpoint: JournalCheckpoint) -> Self {
        Self { checkpoint, ..Default::default() }
    }
}

const VOLUME_DELETED: u64 = u64::MAX;

#[derive(Debug)]
pub struct JournaledChecksums {
    pub device_range: ExtentRange,
    pub checksums: Checksums,
    pub first_write: bool,
}

/// cladefs: whether what a journal record says is allocated could be: not empty, and on blocks, as an
/// allocator's key, which what it says becomes, must be for a layer to hold it.
fn valid_device_range(device_range: &ExtentRange, block_size: BlockSize) -> bool {
    !device_range.is_empty() && block_size.is_aligned(&device_range.range)
}

/// Handles for journal-like streams (the journal itself, super-block extent, and fake objects).
pub trait JournalHandle: ReadObjectHandle {
    /// cladefs: appends an in-stream `JournalSpan` if it extends the current last span matching
    /// `(prev_cell, prev_end_sequence)`, returning the offset from which read-ahead should be
    /// invalidated (board 6923).
    fn try_push_stream_span(
        &mut self,
        span: JournalSpan,
        prev_cell: u64,
        prev_end_sequence: u64,
    ) -> Result<Option<u64>, Error>;
}

impl<S: HandleOwner> JournalHandle for DataObjectHandle<S> {
    fn try_push_stream_span(
        &mut self,
        _span: JournalSpan,
        _prev_cell: u64,
        _prev_end_sequence: u64,
    ) -> Result<Option<u64>, Error> {
        Ok(None)
    }
}

impl Journal {
    pub fn new(
        objects: Arc<ObjectManager>,
        options: JournalOptions,
        hooks: Arc<HooksHandle>,
    ) -> Journal {
        // cladefs: use `getrandom` directly rather than `rand::random_range` so the guest kernel
        // needs no thread-local RNG state (board 6929).
        let starting_checksum = loop {
            let v = getrandom::u64().expect("getrandom failed");
            if v != 0 && v != u64::MAX {
                break v;
            }
        };
        Journal {
            objects,
            hooks,
            handle: OnceLock::new(),
            super_block_manager: SuperBlockManager::new(),
            inner: Mutex::new(Inner {
                super_block_header: SuperBlockHeader::default(),
                position: JournalPosition::default(),
                writer: JournalWriter::new(BLOCK_SIZE, starting_checksum),
                output_reset_version: false,
                flush_waker: None,
                terminate: false,
                terminate_reason: None,
                disable_compactions: false,
                compactions_paused: false,
                compaction_running: false,
                sync_waker: None,
                reclaim_size: options.reclaim_size,
                barriers_enabled: options.barriers_enabled,
                needs_barrier: false,
                forced_compaction: false,
                lazy_flush: options.lazy_flush,
                lazy_flush_armed: false,
            }),
            writer_mutex: Mutex::new(()),
            sync_mutex: futures::lock::Mutex::new(()),
            chunk_size: options.chunk_size,
            reclaim_event: Event::new(),
            stats: JournalStats::default(),
            writer_end_block: AtomicU64::new(0),
        }
    }

    /// cladefs: live and unfreed system extents (active/retired super-block + unfreed journal
    /// blocks) with their sizes in bytes, for leak and byte-counting oracles (boards 6923, 6926).
    pub fn known_extents(&self) -> HashMap<ExtentId, u64> {
        let (sb_extents, journal_freed_to) = self.super_block_manager.known_system();
        let mut out: HashMap<ExtentId, u64> = HashMap::default();
        for (extent, len) in sb_extents {
            out.entry(extent).and_modify(|v| *v = (*v).max(len)).or_insert(len);
        }
        if let Some(handle) = self.handle.get() {
            let end = handle.end_offset();
            for extent in handle.extents_in(journal_freed_to..end) {
                out.entry(extent)
                    .and_modify(|v| *v = (*v).max(BLOCK_SIZE.get()))
                    .or_insert(BLOCK_SIZE.get());
            }
        }
        out
    }

    /// cladefs: physical bytes held (or buffered to be written) by live/unfreed system extents
    /// (super-block + unfreed journal blocks), for allocator capacity accounting (boards 6923, 6926).
    pub fn system_bytes(&self) -> u64 {
        self.super_block_manager.system_bytes(self.writer_end_block.load(Ordering::Relaxed))
    }

    /// cladefs: physical system bytes plus headroom for upcoming journal blocks and a super-block
    /// write, so `Allocator::needs_sync` frees dead extents before a commit or compaction fills
    /// the device (boards 6923, 6926).
    pub fn sync_headroom_bytes(&self) -> u64 {
        self.system_bytes()
            .saturating_add(self.chunk_size)
            .saturating_add(super_block::MIN_SUPER_BLOCK_SIZE)
    }

    fn record_writer_end(&self, file_offset: u64) {
        let aligned = BLOCK_SIZE.align_up(file_offset).unwrap_or(file_offset);
        self.writer_end_block.fetch_max(aligned, Ordering::Relaxed);
    }

    /// cladefs: the number of journal spans currently tracked in memory.
    pub(crate) fn span_count(&self) -> usize {
        self.handle.get().map_or(0, JournalExtentHandle::span_count)
    }

    /// cladefs: whether `Root::journal_spans` has reached `COMPACT_SPAN_THRESHOLD` and should be
    /// compacted right away (after mount or thaw).
    pub(crate) fn needs_span_compaction(&self) -> bool {
        self.span_count() >= COMPACT_SPAN_THRESHOLD
    }

    #[cfg(test)]
    pub(super) fn handle_for_test(&self) -> &JournalExtentHandle {
        self.handle.get().expect("No journal handle")
    }

    /// Used during replay to validate a mutation.  This should return false if the mutation is not
    /// valid and should not be applied.  This could be for benign reasons: e.g. the device flushed
    /// data out-of-order, or because of a malicious actor.
    /// cladefs: an object store's item is what its value says it may be, as of a layer's
    /// (`ObjectValue::is_valid_for`).
    pub(super) fn validate_mutation(mutation: &Mutation, block_size: BlockSize) -> bool {
        match mutation {
            Mutation::ObjectStore(ObjectStoreMutation { item, .. }) => {
                return item.value.is_valid_for(&item.key, block_size);
            }
            Mutation::Allocator(AllocatorMutation::Allocate { device_range, owner_object_id }) => {
                return valid_device_range(device_range, block_size)
                    && *owner_object_id != INVALID_OBJECT_ID;
            }
            Mutation::Allocator(AllocatorMutation::Deallocate {
                device_range,
                owner_object_id,
            }) => {
                return valid_device_range(device_range, block_size)
                    && *owner_object_id != INVALID_OBJECT_ID;
            }
            Mutation::Allocator(AllocatorMutation::Reach { device_range, owner_object_id }) => {
                return valid_device_range(device_range, block_size)
                    && *owner_object_id != INVALID_OBJECT_ID;
            }
            Mutation::Allocator(AllocatorMutation::Reference {
                device_range,
                owner_object_id,
                delta,
            }) => {
                return valid_device_range(device_range, block_size)
                    && *owner_object_id != INVALID_OBJECT_ID
                    && *delta != 0;
            }
            Mutation::Allocator(AllocatorMutation::MarkForDeletion(owner_object_id)) => {
                return *owner_object_id != INVALID_OBJECT_ID;
            }
            Mutation::Allocator(AllocatorMutation::Snapshot {
                source_store_object_id,
                new_store_object_id,
                new_owner_object_id,
            }) => {
                let ids = [source_store_object_id, new_store_object_id, new_owner_object_id];
                return ids.iter().all(|id| **id != INVALID_OBJECT_ID)
                    && source_store_object_id != new_store_object_id
                    && new_store_object_id != new_owner_object_id
                    && source_store_object_id != new_owner_object_id;
            }
            Mutation::BeginFlush => {}
            Mutation::EndFlush => {}
            Mutation::DeleteVolume => {}
            Mutation::UpdateBorrowed { .. } => {}
            Mutation::CreateInternalDir(owner_object_id) => {
                return *owner_object_id != INVALID_OBJECT_ID;
            }
        }
        true
    }

    /// cladefs: the extents whose blocks a replay of `transactions` may read to verify checksums:
    /// those they write, and those whose blocks they change the references to, which may make a
    /// block deallocated before alive again (`update_checksum_list`).
    fn extents_verified_by<'a>(
        transactions: impl Iterator<Item = &'a JournaledTransaction>,
    ) -> HashSet<ExtentId> {
        let mut extents = HashSet::new();
        for transaction in transactions {
            let written = transaction.checksums.iter().map(|checksums| &checksums.device_range);
            extents.extend(written.map(|range| range.extent));
            let mutations = (transaction.root_parent_mutations.iter())
                .chain(&transaction.root_mutations)
                .chain(transaction.non_root_mutations.iter().map(|(_, mutation)| mutation));
            for mutation in mutations {
                if let Mutation::Allocator(
                    AllocatorMutation::Allocate { device_range, .. }
                    | AllocatorMutation::Deallocate { device_range, .. }
                    | AllocatorMutation::Reference { device_range, .. },
                ) = mutation
                {
                    extents.insert(device_range.extent);
                }
            }
        }
        extents
    }

    // Assumes that `mutation` has been validated.
    fn update_checksum_list(
        &self,
        journal_offset: u64,
        mutation: &Mutation,
        checksum_list: &mut ChecksumList,
    ) -> Result<(), Error> {
        match mutation {
            Mutation::ObjectStore(_) => {}
            Mutation::Allocator(AllocatorMutation::Deallocate { device_range, .. }) => {
                checksum_list.mark_deallocated(journal_offset, device_range.clone().into());
            }
            Mutation::Allocator(AllocatorMutation::Reference { device_range, delta, .. }) => {
                checksum_list.mark_referenced(journal_offset, device_range.clone().into(), *delta);
            }
            _ => {}
        }
        Ok(())
    }

    /// Reads the latest super-block, and then replays journaled records.
    pub async fn replay(
        &self,
        filesystem: Arc<FxFilesystem>,
        on_new_allocator: Option<Box<dyn Fn(Arc<Allocator>) + Send + Sync>>,
    ) -> Result<(), Error> {
        Self::check_block_size(&filesystem)?;
        let block_size = filesystem.block_size();

        // cladefs: box large replay stages in non-inlined frames to fit 16 KiB guest kernel stacks
        // (TASK-26.9).
        let (super_block, root_parent) =
            crate::boxed(|| self.super_block_manager.load(filesystem.device(), block_size)).await?;

        let root_parent = ObjectStore::attach_filesystem(root_parent, filesystem.clone());

        self.objects.set_root_parent_store(root_parent.clone());
        let allocator =
            Arc::new(Allocator::new(filesystem.clone(), super_block.allocator_object_id));
        if let Some(on_new_allocator) = on_new_allocator {
            on_new_allocator(allocator.clone());
        }
        self.objects.set_allocator(allocator.clone());
        self.objects.set_borrowed_metadata_space(super_block.borrowed_metadata_space);
        self.objects.set_last_end_offset(super_block.super_block_journal_file_offset);
        {
            let mut inner = self.inner.lock();
            inner.super_block_header = super_block.clone();
        }

        let device = filesystem.device();
        let root = self.super_block_manager.root().await;

        let live_journal_start = BLOCK_SIZE.align_down(super_block.journal_checkpoint.file_offset);
        ensure!(
            !root.journal_spans.is_empty()
                && root.journal_spans[0].start_offset <= live_journal_start
                && live_journal_start < root.journal_spans.last().unwrap().end_offset(),
            anyhow!(FxfsError::Inconsistent).context(format!(
                "Root journal spans do not cover journal checkpoint {live_journal_start}"
            ))
        );

        let handle = JournalExtentHandle::new(device.clone(), root.journal_spans.clone());
        let mut reader = JournalReader::new(handle.clone(), &super_block.journal_checkpoint);
        // cladefs: box in non-inlined frames to keep `Journal::replay`'s stack small (TASK-26.9).
        let JournaledTransactions { mut transactions, device_flushed_offset } =
            crate::boxed(|| self.read_transactions(&mut reader, None, INVALID_OBJECT_ID))
                .await
                .context("Reading transactions for replay")?;

        // Validate all the mutations.
        let mut checksum_list = ChecksumList::new(device_flushed_offset);
        let mut valid_to = reader.journal_file_checkpoint().file_offset;
        'bad_replay: for JournaledTransaction {
            checkpoint,
            root_parent_mutations,
            root_mutations,
            non_root_mutations,
            checksums,
            ..
        } in &transactions
        {
            for JournaledChecksums { device_range, checksums, first_write } in checksums {
                checksum_list
                    .push(
                        checkpoint.file_offset,
                        device_range.clone(),
                        checksums.maybe_as_ref().context("Malformed checksums")?,
                        *first_write,
                    )
                    .context("Pushing journal checksum records to checksum list")?;
            }
            for mutation in root_parent_mutations
                .iter()
                .chain(root_mutations)
                .chain(non_root_mutations.iter().map(|(_, m)| m))
            {
                if !Self::validate_mutation(mutation, block_size) {
                    info!(mutation:?; "Stopping replay at bad mutation");
                    valid_to = checkpoint.file_offset;
                    break 'bad_replay;
                }
                self.update_checksum_list(checkpoint.file_offset, &mutation, &mut checksum_list)?;
            }
        }

        // Validate the checksums. Note if barriers are enabled, there will be no checksums in
        // practice to verify.
        let valid_to = checksum_list
            .verify(device.as_ref(), valid_to)
            .await
            .context("Failed to validate checksums")?;
        // cladefs: what a replay after this one may read again, until the journal records the
        // discard: an extent this replay finds dead mustn't be freed until then (see the
        // allocator's `on_replay_complete`).
        let discarded = Self::extents_verified_by(
            transactions
                .iter()
                .filter(|transaction| transaction.checkpoint.file_offset >= valid_to),
        );

        // Apply the mutations...

        let mut last_checkpoint = reader.journal_file_checkpoint();
        let mut journal_offsets = super_block.journal_file_offsets.clone();

        // Start with the root-parent mutations, and also determine the journal offsets for all
        // other objects.
        for (
            index,
            JournaledTransaction {
                checkpoint,
                root_parent_mutations,
                end_flush,
                volume_deleted,
                ..
            },
        ) in transactions.iter_mut().enumerate()
        {
            if checkpoint.file_offset >= valid_to {
                last_checkpoint = checkpoint.clone();

                // Truncate the transactions so we don't need to worry about them on the next pass.
                transactions.truncate(index);
                break;
            }

            let context = ApplyContext { mode: ApplyMode::Replay, checkpoint: checkpoint.clone() };
            for mutation in root_parent_mutations.drain(..) {
                self.objects
                    .apply_mutation(
                        super_block.root_parent_store_object_id,
                        mutation,
                        &context,
                        AssocObj::None,
                    )
                    .context("Failed to replay root parent store mutations")?;
            }

            if let Some((object_id, journal_offset)) = end_flush {
                journal_offsets.insert(*object_id, *journal_offset);
            }

            if let Some(object_id) = volume_deleted {
                journal_offsets.insert(*object_id, VOLUME_DELETED);
            }
        }

        // Now we can open the root store.
        let root_store = crate::boxed(|| {
            ObjectStore::open(&root_parent, super_block.root_store_object_id, None)
        })
        .await
        .context("Unable to open root store")?;
        self.objects.set_root_store(root_store);

        let root_store_offset =
            journal_offsets.get(&super_block.root_store_object_id).copied().unwrap_or(0);

        // Now replay the root store mutations.
        for JournaledTransaction { checkpoint, root_mutations, .. } in &mut transactions {
            if checkpoint.file_offset < root_store_offset {
                continue;
            }

            let context = ApplyContext { mode: ApplyMode::Replay, checkpoint: checkpoint.clone() };
            for mutation in root_mutations.drain(..) {
                self.objects
                    .apply_mutation(
                        super_block.root_store_object_id,
                        mutation,
                        &context,
                        AssocObj::None,
                    )
                    .context("Failed to replay root store mutations")?;
            }
        }

        // Now we can open the allocator.
        crate::boxed(|| allocator.open()).await.context("Failed to open allocator")?;

        // Now replay all other mutations.
        for JournaledTransaction { checkpoint, non_root_mutations, end_offset, .. } in transactions
        {
            let context = ApplyContext { mode: ApplyMode::Replay, checkpoint };
            crate::boxed(|| {
                self.objects.replay_mutations(
                    non_root_mutations,
                    &journal_offsets,
                    &context,
                    end_offset,
                )
            })
            .await
            .context("Failed to replay mutations")?;
        }

        let discarded_to =
            if last_checkpoint.file_offset != reader.journal_file_checkpoint().file_offset {
                Some(reader.journal_file_checkpoint().file_offset)
            } else {
                None
            };

        // Validate replay bounds BEFORE freeing any orphans or mutating Root on the device.
        if last_checkpoint.file_offset < super_block.super_block_journal_file_offset {
            return Err(anyhow!(FxfsError::Inconsistent).context(format!(
                "journal replay cut short; journal finishes at {}, but super-block was \
                 written at {}",
                last_checkpoint.file_offset, super_block.super_block_journal_file_offset
            )));
        }
        // cladefs: which fails if the block the journal ends in wasn't read: one it started in
        // that didn't verify (TASK-149.23).
        reader.skip_to_end_of_block()?;
        let replay_end = reader.journal_file_checkpoint().file_offset;
        // cladefs: nor may it end before where a barrier last found it written (TASK-153.13,
        // TASK-182).
        let ended = match discarded_to {
            Some(_) => last_checkpoint.file_offset,
            None => replay_end,
        };
        if ended < root.journal_end {
            return Err(anyhow!(FxfsError::Inconsistent).context(format!(
                "journal replay cut short; journal finishes at {ended}, but was written to \
                 {} before a barrier",
                root.journal_end
            )));
        }

        // Classify system extents (super-block + position-derived journal blocks) for orphan
        // sweep and retired-extent reclamation on mount (boards 6923, 6926).
        let spans_discovered = handle.spans();
        let spans_end = spans_discovered.last().map_or(0, JournalSpan::end_offset);
        let cell = device.cell();
        let mut live_system = HashSet::new();
        let mut min_next_sequence = 0u64;
        if let Some(sb) = &root.super_block {
            if sb.extent.cell == cell {
                live_system.insert(sb.extent);
                min_next_sequence = min_next_sequence.max(sb.extent.sequence.saturating_add(1));
            }
        }
        for span in &spans_discovered {
            if span.cell == cell {
                min_next_sequence = min_next_sequence.max(span.end_sequence());
            }
        }
        for extent in extents_in_spans(&spans_discovered, live_journal_start..replay_end) {
            if extent.cell == cell {
                live_system.insert(extent);
            }
        }
        let mut retired_system: Vec<ExtentId> =
            root.retired_super_block.iter().copied().filter(|e| e.cell == cell).collect();
        if live_journal_start > root.journal_freed_to {
            retired_system.extend(
                extents_in_spans(&spans_discovered, root.journal_freed_to..live_journal_start)
                    .into_iter()
                    .filter(|e| e.cell == cell),
            );
        }
        let unused_tail: Vec<ExtentId> = if spans_end > replay_end {
            extents_in_spans(&spans_discovered, replay_end..spans_end)
                .into_iter()
                .filter(|e| e.cell == cell)
                .collect()
        } else {
            Vec::new()
        };

        crate::boxed(|| {
            allocator.on_replay_complete(
                &discarded,
                &live_system,
                &retired_system,
                &unused_tail,
                min_next_sequence,
            )
        })
        .await
        .context("Failed to complete replay for allocator")?;

        // On writable mount, truncate spans at `replay_end`, prune freed blocks before
        // `live_journal_start`, mint a fresh span on `device.cell()`, and commit `Root` once.
        // This replaces `renew_tail` probing, `reallocate_range`, and `record_journal_tail`
        // (board 6923).
        if !filesystem.options().read_only {
            let mut active_spans = spans_discovered;
            prune_spans_before(&mut active_spans, live_journal_start);
            truncate_spans_at(&mut active_spans, replay_end);
            let blocks = self.span_blocks();
            let (span_cell, base_sequence) = allocator.mint_journal_span(blocks)?;
            active_spans.push(JournalSpan {
                start_offset: replay_end,
                block_count: blocks,
                cell: span_cell,
                base_sequence,
            });
            crate::boxed(|| {
                self.super_block_manager.reset_after_mount_commit(
                    active_spans.clone(),
                    live_journal_start,
                    ended,
                    &*device,
                )
            })
            .await?;
            handle.replace_spans(active_spans);
        }

        // Configure the journal writer so that we can continue.
        {
            let _ = self.handle.set(handle);
            let mut inner = self.inner.lock();
            let mut writer_checkpoint = reader.journal_file_checkpoint();

            // Make sure we don't accidentally use the reader from now onwards.
            std::mem::drop(reader);

            // Reset the stream to indicate that we've remounted the journal.
            writer_checkpoint.checksum ^= RESET_XOR;
            writer_checkpoint.version = LATEST_VERSION;
            inner.position.replayed(writer_checkpoint.file_offset, last_checkpoint.file_offset);

            self.record_writer_end(writer_checkpoint.file_offset);
            inner.writer.seek(writer_checkpoint);
            inner.output_reset_version = true;
        }

        // cladefs: box in a non-inlined frame (already inside offloaded `replay`) (TASK-26.9).
        crate::boxed(|| self.objects.on_replay_complete())
            .await
            .context("Failed to complete replay for object manager")?;

        info!(checkpoint = last_checkpoint.file_offset, discarded_to; "replay complete");
        Ok(())
    }

    async fn read_transactions(
        &self,
        reader: &mut JournalReader,
        end_offset: Option<u64>,
        object_id_filter: u64,
    ) -> Result<JournaledTransactions, Error> {
        let mut transactions = Vec::new();
        let (mut device_flushed_offset, root_parent_store_object_id, root_store_object_id) = {
            let super_block = &self.inner.lock().super_block_header;
            (
                super_block.super_block_journal_file_offset,
                super_block.root_parent_store_object_id,
                super_block.root_store_object_id,
            )
        };
        let mut current_transaction = None;
        let mut begin_flush_offsets = HashMap::default();
        let mut stores_deleted = HashSet::new();
        loop {
            // Cache the checkpoint before we deserialize a record.
            let checkpoint = reader.journal_file_checkpoint();
            if let Some(end_offset) = end_offset {
                if checkpoint.file_offset >= end_offset {
                    break;
                }
            }
            let result =
                reader.deserialize().await.context("Failed to deserialize journal record")?;
            match result {
                ReadResult::Reset(version) => {
                    // cladefs: refuse journal reset records outside
                    // EARLIEST_SUPPORTED_VERSION..=LATEST_VERSION (doc-28, board 10050).
                    ensure!(
                        (EARLIEST_SUPPORTED_VERSION..=LATEST_VERSION).contains(&version),
                        anyhow!(FxfsError::InvalidVersion)
                            .context(format!("Unsupported Journal RESET version: {version:?}"))
                    );
                    if current_transaction.is_some() {
                        current_transaction = None;
                        transactions.pop();
                    }
                    let offset = reader.journal_file_checkpoint().file_offset;
                    if offset > device_flushed_offset {
                        device_flushed_offset = offset;
                    }
                }
                ReadResult::Some(record) => {
                    match record {
                        JournalRecord::EndBlock => {
                            reader.skip_to_end_of_block()?;
                        }
                        JournalRecord::Mutation { object_id, mutation } => {
                            let current_transaction = match current_transaction.as_mut() {
                                None => {
                                    transactions.push(JournaledTransaction::new(checkpoint));
                                    current_transaction = transactions.last_mut();
                                    current_transaction.as_mut().unwrap()
                                }
                                Some(transaction) => transaction,
                            };

                            if stores_deleted.contains(&object_id) {
                                bail!(
                                    anyhow!(FxfsError::Inconsistent)
                                        .context("Encountered mutations for deleted store")
                                );
                            }

                            match &mutation {
                                Mutation::BeginFlush => {
                                    begin_flush_offsets.insert(
                                        object_id,
                                        current_transaction.checkpoint.file_offset,
                                    );
                                }
                                Mutation::EndFlush => {
                                    if let Some(offset) = begin_flush_offsets.remove(&object_id) {
                                        if let Some(deleted_volume) =
                                            &current_transaction.volume_deleted
                                        {
                                            if *deleted_volume == object_id {
                                                bail!(anyhow!(FxfsError::Inconsistent).context(
                                                    "Multiple EndFlush/DeleteVolume mutations in a \
                                                    single transaction for the same object"
                                                ));
                                            }
                                        }
                                        // The +1 is because we don't want to replay the transaction
                                        // containing the begin flush; we don't need or want to
                                        // replay it.
                                        if current_transaction
                                            .end_flush
                                            .replace((object_id, offset + 1))
                                            .is_some()
                                        {
                                            bail!(anyhow!(FxfsError::Inconsistent).context(
                                                "Multiple EndFlush mutations in a \
                                                 single transaction"
                                            ));
                                        }
                                    }
                                }
                                Mutation::DeleteVolume => {
                                    if let Some((flushed_object, _)) =
                                        &current_transaction.end_flush
                                    {
                                        if *flushed_object == object_id {
                                            bail!(anyhow!(FxfsError::Inconsistent).context(
                                                "Multiple EndFlush/DeleteVolume mutations in a \
                                                    single transaction for the same object"
                                            ));
                                        }
                                    }
                                    if current_transaction
                                        .volume_deleted
                                        .replace(object_id)
                                        .is_some()
                                    {
                                        bail!(anyhow!(FxfsError::Inconsistent).context(
                                            "Multiple DeleteVolume mutations in a single \
                                             transaction"
                                        ));
                                    }
                                    stores_deleted.insert(object_id);
                                }
                                _ => {}
                            }

                            // If this mutation doesn't need to be applied, don't bother adding it
                            // to the transaction.
                            if (object_id_filter == INVALID_OBJECT_ID
                                || object_id_filter == object_id)
                                && self.should_apply(object_id, &current_transaction.checkpoint)
                            {
                                if object_id == root_parent_store_object_id {
                                    current_transaction.root_parent_mutations.push(mutation);
                                } else if object_id == root_store_object_id {
                                    current_transaction.root_mutations.push(mutation);
                                } else {
                                    current_transaction
                                        .non_root_mutations
                                        .push((object_id, mutation));
                                }
                            }
                        }
                        JournalRecord::DataChecksums(device_range, checksums, first_write) => {
                            let current_transaction = match current_transaction.as_mut() {
                                None => {
                                    transactions.push(JournaledTransaction::new(checkpoint));
                                    current_transaction = transactions.last_mut();
                                    current_transaction.as_mut().unwrap()
                                }
                                Some(transaction) => transaction,
                            };
                            current_transaction.checksums.push(JournaledChecksums {
                                device_range,
                                checksums,
                                first_write,
                            });
                        }
                        JournalRecord::Commit => {
                            if let Some(&mut JournaledTransaction { ref mut end_offset, .. }) =
                                current_transaction.take()
                            {
                                *end_offset = reader.journal_file_checkpoint().file_offset;
                            }
                        }
                        JournalRecord::Discard(offset) => {
                            if offset == 0 {
                                bail!(
                                    anyhow!(FxfsError::Inconsistent)
                                        .context("Invalid offset for Discard")
                                );
                            }
                            if let Some(transaction) = current_transaction.as_ref() {
                                if transaction.checkpoint.file_offset < offset {
                                    // Odd, but OK.
                                    continue;
                                }
                            }
                            current_transaction = None;
                            while let Some(transaction) = transactions.last() {
                                if transaction.checkpoint.file_offset < offset {
                                    break;
                                }
                                transactions.pop();
                            }
                        }
                        JournalRecord::DidFlushDevice(offset) => {
                            if offset > device_flushed_offset {
                                device_flushed_offset = offset;
                            }
                        }
                        JournalRecord::JournalSpan { span, prev_cell, prev_end_sequence } => {
                            reader.try_push_stream_span(span, prev_cell, prev_end_sequence)?;
                        }
                    }
                }
                // This is expected when we reach the end of the journal stream.
                ReadResult::ChecksumMismatch => break,
            }
        }

        // Discard any uncommitted transaction.
        if current_transaction.is_some() {
            transactions.pop();
        }

        Ok(JournaledTransactions { transactions, device_flushed_offset })
    }

    /// Creates an empty filesystem with the minimum viable objects (including a root parent and
    /// root store but no further child stores).
    pub async fn init_empty(&self, filesystem: Arc<FxFilesystem>) -> Result<(), Error> {
        Self::check_block_size(&filesystem)?;
        // The following constants are only used at format time. When mounting, the recorded values
        // in the superblock should be used.  The root parent store does not have a parent, but
        // needs an object ID to be registered with ObjectManager, so it cannot collide (i.e. have
        // the same object ID) with any objects in the root store that use the journal to track
        // mutations.
        const INIT_ROOT_PARENT_STORE_OBJECT_ID: u64 = 3;
        const INIT_ROOT_STORE_OBJECT_ID: u64 = 4;
        const INIT_ALLOCATOR_OBJECT_ID: u64 = 5;

        info!(device_size = filesystem.device().size(); "Formatting");

        let checkpoint = JournalCheckpoint {
            version: LATEST_VERSION,
            ..self.inner.lock().writer.journal_file_checkpoint()
        };

        let root_parent = ObjectStore::new_empty(
            None,
            INIT_ROOT_PARENT_STORE_OBJECT_ID,
            filesystem.clone(),
            None,
        );
        self.objects.set_root_parent_store(root_parent.clone());

        let allocator = Arc::new(Allocator::new(filesystem.clone(), INIT_ALLOCATOR_OBJECT_ID));
        self.objects.set_allocator(allocator.clone());
        self.objects.init_metadata_reservation()?;

        let root_store;
        let mut transaction = crate::boxed(|| {
            root_parent.new_transaction(
                lock_keys![],
                Options { skip_journal_checks: true, ..Default::default() },
            )
        })
        .await?;
        root_store = crate::boxed(|| {
            root_parent.new_child_store(
                &mut transaction,
                NewChildStoreOptions { object_id: INIT_ROOT_STORE_OBJECT_ID, ..Default::default() },
                None,
            )
        })
        .await
        .context("new_child_store")?;
        self.objects.set_root_store(root_store.clone());

        crate::boxed(|| allocator.create(&mut transaction)).await?;

        // Write the root store object info.
        crate::boxed(|| root_store.create(&mut transaction)).await?;

        // The root parent graveyard.
        root_parent.set_graveyard_directory_object_id(
            crate::boxed(|| Graveyard::create(&mut transaction, &root_parent)).await?,
        );

        crate::boxed(|| transaction.commit()).await?;

        self.inner.lock().super_block_header = SuperBlockHeader::new(
            1,
            root_parent.store_object_id(),
            root_parent.graveyard_directory_object_id(),
            root_store.store_object_id(),
            allocator.object_id(),
            checkpoint,
            /* earliest_version: */ LATEST_VERSION,
        );

        self.init_journal_handle(&filesystem);
        Ok(())
    }

    fn init_journal_handle(&self, filesystem: &FxFilesystem) {
        let blocks = self.span_blocks();
        let (cell, base_sequence) = self
            .objects
            .allocator()
            .mint_journal_span(blocks)
            .expect("initial journal span sequence overflow");
        let initial = JournalSpan { start_offset: 0, block_count: blocks, cell, base_sequence };
        let handle = JournalExtentHandle::new(filesystem.device(), vec![initial]);
        let _ = self.handle.set(handle);
    }

    /// cladefs: fails unless the filesystem's blocks are the journal's, so that each journal block
    /// is a 4 KiB write-once extent of its own.
    fn check_block_size(filesystem: &FxFilesystem) -> Result<(), Error> {
        ensure!(
            filesystem.block_size() == BLOCK_SIZE,
            anyhow!(FxfsError::NotSupported)
                .context(format!("Blocks of {} bytes, not the journal's", filesystem.block_size()))
        );
        Ok(())
    }

    /// cladefs: as a frozen filesystem thaws in a VM restored from a snapshot onto a forked cell,
    /// rotates the journal to a fresh span on the new cell in `Root` without rewriting the
    /// super-block (board 6923). No-op when the cell has not changed.
    pub async fn rotate_journal_span(&self) -> Result<(), Error> {
        let handle = self.handle.get().expect("No journal handle");
        let device = handle.device().clone();
        let cell = device.cell();
        if handle.last_span().is_some_and(|s| s.cell == cell) {
            return Ok(());
        }
        self.objects.allocator().advance_sequence_floor().await?;
        let at = {
            let inner = self.inner.lock();
            let offset = inner.writer.journal_file_checkpoint().file_offset;
            debug_assert!(
                BLOCK_SIZE.is_aligned(offset) && offset == inner.position.flushed_offset(),
                "thaw rotating journal at {offset}, flushed {}",
                inner.position.flushed_offset()
            );
            offset
        };
        let mut spans = handle.spans();
        truncate_spans_at(&mut spans, at);
        let blocks = self.span_blocks();
        let (span_cell, base_sequence) = self.objects.allocator().mint_journal_span(blocks)?;
        spans.push(JournalSpan {
            start_offset: at,
            block_count: blocks,
            cell: span_cell,
            base_sequence,
        });
        let spans_for_root = spans.clone();
        self.super_block_manager
            .set_root(
                |root| {
                    let mut root_spans = spans_for_root;
                    prune_spans_before(&mut root_spans, root.journal_freed_to);
                    root.journal_spans = root_spans;
                    root.journal_end = root.journal_end.max(at);
                },
                async |root| device.commit_root(&root).await.map(|()| true),
            )
            .await
            .context("committing rotated journal span on thaw")?;
        // Only switch the in-memory handle AFTER `commit_root` succeeds so a failed thaw retry
        // (`TASK-200`) still sees the old cell and re-commits `Root`.
        handle.replace_spans(spans);
        Ok(())
    }

    /// Writes the single initial super-block after formatting (board 6926).
    pub async fn init_superblock(&self) -> Result<(), Error> {
        self.write_super_block().await
    }

    /// Takes a snapshot of all journaled transactions which affect |object_id| since its last
    /// flush.
    /// The caller is responsible for locking; it must ensure that the journal is not trimmed during
    /// this call.  For example, a Flush lock could be held on the object in question (assuming that
    /// object has data to flush and is registered with ObjectManager).
    pub async fn read_transactions_for_object(
        &self,
        object_id: u64,
    ) -> Result<Vec<JournaledTransaction>, Error> {
        let handle = self.handle.get().expect("No journal handle").clone();

        let checkpoint = match self.objects.journal_checkpoint(object_id) {
            Some(checkpoint) => checkpoint,
            None => return Ok(vec![]),
        };
        let mut reader = JournalReader::new(handle, &checkpoint);
        // Record the current end offset and only read to there, so we don't accidentally read any
        // partially flushed blocks.
        let end_offset = self.inner.lock().position.valid_to();
        Ok(self.read_transactions(&mut reader, Some(end_offset), object_id).await?.transactions)
    }

    /// Commits a transaction.  This is not thread safe; the caller must take appropriate locks.
    pub async fn commit(&self, transaction: &mut Transaction<'_>) -> Result<u64, Error> {
        if transaction.is_empty() {
            return Ok(self.inner.lock().writer.journal_file_checkpoint().file_offset);
        }

        // cladefs: box `pre_commit` in a non-inlined frame so `Journal::commit` stays small on
        // 16 KiB guest kernel stacks during normal commits (TASK-26.9).
        crate::boxed_pin(|| self.pre_commit(transaction)).await?;
        self.write_and_apply_mutations(transaction)
    }

    /// Ensures the active journal handle has at least `room` bytes of span capacity ahead of the
    /// current writer offset, minting and emitting an in-stream `JournalRecord::JournalSpan`
    /// while still inside the current span (board 6923). Caller must hold `writer_mutex`.
    fn ensure_span_room(&self, inner: &mut Inner, room: u64) -> Result<(), Error> {
        let Some(handle) = self.handle.get() else {
            return Ok(());
        };
        let file_offset = inner.writer.journal_file_checkpoint().file_offset;
        let end_offset = handle.end_offset();
        if file_offset + room <= end_offset {
            return Ok(());
        }
        ensure!(
            handle.span_count() < super_block::MAX_ROOT_SPANS,
            anyhow!(FxfsError::NoSpace)
                .context("Too many live journal spans in root; compaction required")
        );
        let needed_bytes = (file_offset + room) - end_offset;
        let blocks = self.span_blocks().max(needed_bytes.div_ceil(BLOCK_SIZE.get()));
        ensure!(
            blocks <= super_block::MAX_SPAN_BLOCKS,
            anyhow!(FxfsError::NoSpace).context("Journal span exceeds MAX_SPAN_BLOCKS")
        );
        let (prev_cell, prev_end_sequence) =
            handle.last_end_key().expect("journal handle has at least one span");
        let (cell, base_sequence) = self.objects.allocator().mint_journal_span(blocks)?;
        let new_span =
            JournalSpan { start_offset: end_offset, block_count: blocks, cell, base_sequence };
        // Push span to handle BEFORE writing the record so `flush` always finds it.
        handle.push_span(new_span.clone());
        inner.writer.write_record(&JournalRecord::JournalSpan {
            span: new_span,
            prev_cell,
            prev_end_sequence,
        })?;
        self.record_writer_end(inner.writer.journal_file_checkpoint().file_offset);
        Ok(())
    }

    /// Before committing, extends the journal with a new position-derived `JournalSpan` record
    /// in-stream if needed (no transaction or allocator LSM mutation!), writes pending framing
    /// records, and accounts for accumulated sync padding if needed.
    async fn pre_commit(&self, transaction: &Transaction<'_>) -> Result<(), Error> {
        let room = self.room_for(transaction);
        let needs_accounting = {
            let _guard = self.writer_mutex.lock();
            let mut inner = self.inner.lock();
            self.ensure_span_room(&mut inner, room)?;
            Self::write_pending_records(&mut inner)?;
            if self.handle.get().is_none() {
                return Ok(());
            }
            let file_offset = inner.writer.journal_file_checkpoint().file_offset;
            if self.objects.needs_borrow_for_journal(file_offset) {
                Some(self.objects.unaccounted_journal(file_offset))
            } else {
                None
            }
        };

        if let Some(unaccounted) = needs_accounting {
            let root_parent = self.objects.root_parent_store();
            let borrowing = Options {
                skip_journal_checks: true,
                borrow_metadata_space: true,
                allocator_reservation: Some(self.objects.metadata_reservation()),
                ..Default::default()
            };
            let pays = unaccounted < TRANSACTION_MAX_JOURNAL_USAGE / 2;
            let mut txn = if pays {
                let paying = Options { skip_journal_checks: true, ..Default::default() };
                match root_parent.new_transaction(lock_keys![], paying).await {
                    Err(error) if FxfsError::NoSpace.matches(&error) => {
                        root_parent.new_transaction(lock_keys![], borrowing).await?
                    }
                    txn => txn?,
                }
            } else {
                root_parent.new_transaction(lock_keys![], borrowing).await?
            };
            txn.holding_commits = true;
            self.write_and_apply_mutations(&mut txn)?;
        }
        Ok(())
    }

    /// Writes the records pending before the next transaction's.
    fn write_pending_records(inner: &mut Inner) -> Result<(), Error> {
        // If this is the first write after a RESET, we need to output version first.
        if std::mem::take(&mut inner.output_reset_version) {
            LATEST_VERSION.serialize_into(&mut inner.writer)?;
        }

        if let Some(discard_offset) = inner.position.take_discard() {
            JournalRecord::Discard(discard_offset).serialize_into(&mut inner.writer)?;
        }

        if let Some(offset) = inner.position.take_did_flush_device() {
            JournalRecord::DidFlushDevice(offset).serialize_into(&mut inner.writer)?;
        }
        Ok(())
    }

    // Determines whether a mutation at the given checkpoint should be applied.  During replay, not
    // all records should be applied because the object store or allocator might already contain the
    // mutation.  After replay, that obviously isn't the case and we want to apply all mutations.
    fn should_apply(&self, object_id: u64, journal_file_checkpoint: &JournalCheckpoint) -> bool {
        let super_block_header = &self.inner.lock().super_block_header;
        let offset = super_block_header
            .journal_file_offsets
            .get(&object_id)
            .cloned()
            .unwrap_or(super_block_header.super_block_journal_file_offset);
        journal_file_checkpoint.file_offset >= offset
    }

    /// cladefs: how far the journal is written, between commits, which each move it, and whose
    /// changes are all applied: what was read since hasn't changed if it is where it was.
    pub fn file_offset(&self) -> u64 {
        let _guard = self.writer_mutex.lock();
        self.inner.lock().writer.journal_file_checkpoint().file_offset
    }

    /// Flushes previous writes to the device and then atomically writes a new super-block in
    /// `Root`, freeing the superseded super-block and superseded journal blocks afterwards.
    /// Callers must ensure that we do not make concurrent calls.
    pub(crate) async fn write_super_block(&self) -> Result<(), Error> {
        // cladefs: record any dropped shared-extent reaches before `flush_device` /
        // `super_block_manager.save` seals extents on the device (board 11651).
        if self.objects.allocator().has_unrecorded_reaches() {
            let allocator = self.objects.allocator();
            crate::boxed(|| allocator.record_reaches()).await?;
        }

        let root_parent_store = self.objects.root_parent_store();

        let old_layers;
        let mut new_super_block_header;
        let checkpoint;
        let borrowed;
        let journal_file_offsets;
        let min_checkpoint;

        {
            let _sync_guard = debug_assert_not_too_long!(self.sync_mutex.lock());
            {
                let _write_guard = self.writer_mutex.lock();
                (checkpoint, borrowed) = self.pad_to_block()?;
                old_layers = super_block::compact_root_parent(&*root_parent_store)?;
                (journal_file_offsets, min_checkpoint) = self.objects.journal_file_offsets();
            }
            crate::boxed(|| self.flush_device(checkpoint.file_offset, false))
                .await
                .context("flush failed when writing superblock")?;
        }

        new_super_block_header = self.inner.lock().super_block_header.clone();

        new_super_block_header.generation = new_super_block_header.generation.wrapping_add(1);
        new_super_block_header.super_block_journal_file_offset = checkpoint.file_offset;
        new_super_block_header.journal_checkpoint = min_checkpoint.unwrap_or(checkpoint);
        new_super_block_header.journal_checkpoint.version = LATEST_VERSION;
        new_super_block_header.journal_file_offsets = journal_file_offsets;
        new_super_block_header.borrowed_metadata_space = borrowed;

        let handle = self.handle.get().expect("No journal handle");
        let valid_to = self.inner.lock().position.valid_to();
        crate::boxed(|| {
            self.super_block_manager.save(
                new_super_block_header.clone(),
                self.objects.root_parent_store().filesystem(),
                old_layers,
                handle,
                valid_to,
            )
        })
        .await?;
        {
            let mut inner = self.inner.lock();
            inner.super_block_header = new_super_block_header;
        }

        Ok(())
    }

    /// Flushes any buffered journal data to the device.  Note that this does not flush the device
    /// unless the flush_device option is set, in which case data should have been persisted to
    /// lower layers.  If a precondition is supplied, it is evaluated and the sync will be skipped
    /// if it returns false.  This allows callers to check a condition whilst a lock is held.  If a
    /// sync is performed, this function returns the checkpoint that was flushed and the amount of
    /// borrowed metadata space at the point it was flushed.
    pub async fn sync(
        &self,
        options: SyncOptions<'_>,
    ) -> Result<Option<(JournalCheckpoint, u64)>, Error> {
        let _guard = debug_assert_not_too_long!(self.sync_mutex.lock());

        let ((checkpoint, borrowed), sync) = {
            if let Some(precondition) = options.precondition {
                if !precondition() {
                    return Ok(None);
                }
            }

            // This guard is required so that we don't insert an EndBlock record in the middle of a
            // transaction.
            let _guard = self.writer_mutex.lock();

            let padded = self.pad_to_block()?;
            let sync = options.flush_device.then(|| {
                let mut inner = self.inner.lock();
                let end = inner.writer.journal_file_checkpoint().file_offset;
                inner.position.begin_sync(end)
            });
            (padded, sync)
        };

        if let Some(sync) = sync {
            self.flush_device(checkpoint.file_offset, false).await.context("sync: flush failed")?;
            self.inner.lock().position.end_sync(sync);
        }

        Ok(Some((checkpoint, borrowed)))
    }

    /// cladefs: flushes the device, though the journal has nothing more for it, for the extents
    /// that wait for a flush, frees them, and flushes it again, which seals their frees: a mount's
    /// orphans, the super-block it wrote having flushed the journal, stayed on the device until
    /// something else was written, in room Fxfs doesn't count, and a compaction before the seal
    /// recorded them, in room no reservation counts. The flush task must be running, as a flush's
    /// record is written by it.
    pub async fn flush_for_frees(&self) -> Result<(), Error> {
        let _guard = debug_assert_not_too_long!(self.sync_mutex.lock());
        let (checkpoint, _) = {
            let _guard = self.writer_mutex.lock();
            self.pad_to_block()?
        };
        let flushed = self.flush_device(checkpoint.file_offset, true).await?;
        self.flush_device(flushed, true).await?;
        self.objects.allocator().freed().await.context("freeing dead extents")
    }

    // Returns the checkpoint as it was prior to padding.  This is done because the super block
    // needs to record where the last transaction ends and it's the next transaction that pays the
    // price of the padding.
    fn pad_to_block(&self) -> Result<(JournalCheckpoint, u64), Error> {
        let mut inner = self.inner.lock();
        let checkpoint = inner.writer.journal_file_checkpoint();
        if checkpoint.file_offset % BLOCK_SIZE != 0 {
            JournalRecord::EndBlock.serialize_into(&mut inner.writer)?;
            inner.writer.pad_to_block()?;
            self.record_writer_end(inner.writer.journal_file_checkpoint().file_offset);
            if let Some(waker) = inner.flush_waker.take() {
                waker.wake();
            }
        }
        Ok((checkpoint, self.objects.borrowed_metadata_space()))
    }

    /// Flushes the device, `always`, or if the journal has written more than was flushed.
    /// Returns the journal offset flushed through (which may advance past `checkpoint_offset` if
    /// pending shared-extent reaches were recorded before sealing).
    async fn flush_device(&self, mut checkpoint_offset: u64, always: bool) -> Result<u64, Error> {
        self.wait_for_flush(checkpoint_offset).await?;
        // cladefs: the frees the last flush let die are sealed by this one, as they would be if it
        // had waited for them; and a sync for room to allocate in waits for them here.
        self.objects.allocator().freed().await.context("flush: freeing dead extents failed")?;

        let mut flush = self.inner.lock().position.flushing(checkpoint_offset);
        if !flush.durable || always {
            // cladefs: when buffered journal blocks were already flushed without a barrier (or
            // during `flush_for_frees`), `Journal::flush` did not run above. Record any dropped
            // shared-extent reaches and wait for their journal block to flush BEFORE
            // `flush_device_recording()` issues `commit_root` and seals extents (board 11651).
            if !flush.barriered {
                let allocator = self.objects.allocator();
                if !allocator.filesystem_closed() && allocator.has_unrecorded_reaches() {
                    crate::boxed(|| allocator.record_reaches_for_flush()).await?;
                    let (reach_cp, _) = {
                        let _guard = self.writer_mutex.lock();
                        self.pad_to_block()?
                    };
                    checkpoint_offset = checkpoint_offset.max(reach_cp.file_offset);
                    self.wait_for_flush(checkpoint_offset).await?;
                    flush = self.inner.lock().position.flushing(checkpoint_offset);
                }
            }
            if !flush.barriered {
                self.objects.allocator().will_flush_device();
                self.flush_device_recording().await?;
            }

            self.inner.lock().position.device_flushed(checkpoint_offset, flush);

            let allocator = self.objects.allocator();
            let recorded = allocator.needs_recorded_flush(checkpoint_offset);
            if recorded {
                self.record_device_flush().await.context("recording the device flush")?;
            }

            allocator.did_flush_device(checkpoint_offset, recorded).await;
        }

        Ok(checkpoint_offset)
    }

    /// cladefs: flushes the device, recording in `Root` how far the journal is written (`journal_end`),
    /// syncing any newly minted `journal_spans`, and pruning any retired extents sealed by this
    /// barrier (boards 6923, 6926; TASK-153.13).
    async fn flush_device_recording(&self) -> Result<(), Error> {
        let handle = self.handle.get().unwrap();
        let written = self.inner.lock().position.valid_to();
        let device = handle.device().clone();
        let spans = handle.spans();
        let update = |root: &mut Root| {
            root.journal_end = root.journal_end.max(written);
            let mut next_spans = spans;
            prune_spans_before(&mut next_spans, root.journal_freed_to);
            root.journal_spans = next_spans;
        };
        let barrier = async |root: Vec<u8>| device.commit_root(&root).await.map(|()| true);
        self.super_block_manager.set_root(update, barrier).await.map(drop)
    }

    /// cladefs: writes the pending DidFlushDevice record now, and flushes the device again, so that
    /// the record is on it. Ensures span capacity first so repeated recorded flushes can never
    /// overrun the active journal span (board 6923).
    async fn record_device_flush(&self) -> Result<(), Error> {
        let checkpoint = {
            let _guard = self.writer_mutex.lock();
            {
                let mut inner = self.inner.lock();
                self.ensure_span_room(&mut inner, 2 * BLOCK_SIZE.get())?;
                Self::write_pending_records(&mut inner)?;
            }
            self.pad_to_block()?.0
        };
        self.wait_for_flush(checkpoint.file_offset).await?;
        self.handle.get().unwrap().device().flush().await
    }

    /// Waits for the journal to be written to the device up to `offset`.
    pub(crate) async fn wait_for_flush(&self, offset: u64) -> Result<(), Error> {
        debug_assert_not_too_long!(poll_fn(|ctx| {
            let mut inner = self.inner.lock();
            if inner.position.flushed_offset() >= offset {
                Poll::Ready(Ok(()))
            } else if inner.terminate {
                let context = inner
                    .terminate_reason
                    .as_ref()
                    .map(|e| format!("Journal closed with error: {:?}", e))
                    .unwrap_or_else(|| "Journal closed".to_string());
                Poll::Ready(Err(anyhow!(FxfsError::JournalFlushError).context(context)))
            } else {
                inner.sync_waker = Some(ctx.waker().clone());
                // cladefs: a lazy flush task writes what is waited for at once.
                let end = inner.writer.journal_file_checkpoint().file_offset;
                if inner.position.want_flushed(offset, end)
                    && let Some(waker) = inner.flush_waker.take()
                {
                    waker.wake();
                }
                Poll::Pending
            }
        }))
    }

    /// Returns a copy of the super-block header.
    pub fn super_block_header(&self) -> SuperBlockHeader {
        self.inner.lock().super_block_header.clone()
    }

    /// Waits for there to be sufficient space in the journal.
    pub async fn check_journal_space(&self) -> Result<(), Error> {
        // cladefs: when it began waiting, if it did (`stats`).
        let mut waited: Option<std::time::Instant> = None;
        let result = loop {
            let listener = {
                let inner = self.inner.lock();
                if inner.terminate {
                    // If the flush error is set, this will never make progress, since we can't
                    // extend the journal any more.
                    let context = inner
                        .terminate_reason
                        .as_ref()
                        .map(|e| format!("Journal closed with error: {:?}", e))
                        .unwrap_or_else(|| "Journal closed".to_string());
                    break Err(anyhow!(FxfsError::JournalFlushError).context(context));
                }
                if self.objects.last_end_offset()
                    - inner.super_block_header.journal_checkpoint.file_offset
                    < inner.reclaim_size
                {
                    break Ok(());
                }
                if inner.disable_compactions {
                    break Err(
                        anyhow!(FxfsError::JournalFlushError).context("Compactions disabled")
                    );
                }
                self.reclaim_event.listen()
            };
            waited.get_or_insert_with(std::time::Instant::now);
            self.hooks.on_waiting_for_journal_space();
            debug_assert_not_too_long!(listener);
        };
        if let Some(began) = waited {
            self.stats.space_waits.fetch_add(1, Ordering::Relaxed);
            let ns = began.elapsed().as_nanos() as u64;
            self.stats.space_wait_ns.fetch_add(ns, Ordering::Relaxed);
        }
        result
    }

    fn span_blocks(&self) -> u64 {
        (self.chunk_size / BLOCK_SIZE.get()).max(MIN_SPAN_BLOCKS)
    }

    /// cladefs: how much journal space to keep ahead of `writer` before committing `transaction`,
    /// including checksum overhead, a `JournalSpan` record, and any padding-accounting record.
    fn room_for(&self, transaction: &Transaction<'_>) -> u64 {
        let bytes = transaction.journal_bytes();
        // With block checksums, commit/span framing, and a 2-block margin so the next
        // `JournalRecord::JournalSpan` itself always fits inside the current span.
        (bytes + bytes / 8 + 2 * BLOCK_SIZE.get()).next_multiple_of(BLOCK_SIZE.get())
    }

    /// cladefs: serializes `transaction`'s records before the writer's lock, which then only orders
    /// them: the critical section of every commit, which commits running at once wait for.
    fn serialize_records(&self, transaction: &Transaction<'_>) -> SerializedRecords {
        let mut records = SerializedRecords::default();
        let mut iter = transaction.mutations().iter().peekable();
        while let Some(store_iter) = ObjectMutationIterator::new(&mut iter) {
            let object_id = store_iter.object_id();
            for mutation in store_iter {
                records.push(object_id, mutation);
            }
        }
        records
    }

    /// cladefs: commits a transaction directly under `writer_mutex` without re-acquiring
    /// `commit_mutex`, ensuring span room and pending framing records first (used by
    /// `Allocator::record_reaches_holding_commits` / `record_reaches_for_flush`; board 7617,
    /// board 11651).
    pub(crate) fn commit_holding_commits(
        &self,
        transaction: &mut Transaction<'_>,
    ) -> Result<u64, Error> {
        if transaction.is_empty() {
            return Ok(self.inner.lock().writer.journal_file_checkpoint().file_offset);
        }
        self.write_and_apply_mutations(transaction)
    }

    fn write_and_apply_mutations(&self, transaction: &mut Transaction<'_>) -> Result<u64, Error> {
        ensure!(
            self.inner.lock().terminate_reason.is_none(),
            anyhow!(FxfsError::JournalFlushError).context("The journal stopped on an error")
        );
        let room = self.room_for(transaction);
        let serialized = self.serialize_records(transaction);
        let checkpoint_before;
        let checkpoint_after;
        {
            let _guard = self.writer_mutex.lock();
            // cladefs: what flushing the objects it dirties first takes, reserved before it is
            // applied, under the lock that keeps the objects dirty as they are until then (board
            // 14243).
            let flush_charge = self.objects.reserve_flush_charge(transaction);
            checkpoint_before = {
                let mut inner = self.inner.lock();
                // cladefs: re-check span room under `writer_mutex` so a concurrent
                // `commit_holding_commits` (e.g. `record_reaches_for_flush`) advancing the writer
                // between `pre_commit` and here can never overrun the active span (board 11651).
                self.ensure_span_room(&mut inner, room)?;
                Self::write_pending_records(&mut inner)?;
                if transaction.includes_write() {
                    inner.needs_barrier = true;
                }
                let checkpoint = inner.writer.journal_file_checkpoint();
                for record in serialized.records() {
                    inner.writer.write_serialized(record).unwrap();
                }
                checkpoint
            };
            // cladefs: this fails on what memory holds of a corrupt image, such as a transaction
            // that frees what the allocator says isn't allocated, which is no reason for a guest's
            // kernel to panic, as upstream does. Nothing can go on: the filesystem is left as this
            // left it, so the journal stops. What was written of this transaction has no Commit
            // record, which replay discards it for.
            let applied =
                self.objects.apply_transaction(transaction, &checkpoint_before, flush_charge);
            let maybe_mutation = applied.inspect_err(|error| {
                let reason = anyhow!(FxfsError::Inconsistent).context(format!("{error:#}"));
                self.inner.lock().terminate(Some(reason.context("Failed to apply a transaction")));
            })?;
            checkpoint_after = {
                let mut inner = self.inner.lock();
                if let Some(mutation) = maybe_mutation {
                    inner
                        .writer
                        .write_record(&JournalRecord::Mutation { object_id: 0, mutation })
                        .unwrap();
                }
                for (device_range, checksums, first_write) in
                    transaction.take_checksums().into_iter()
                {
                    inner
                        .writer
                        .write_record(&JournalRecord::DataChecksums(
                            device_range,
                            Checksums::fletcher(checksums),
                            first_write,
                        ))
                        .unwrap();
                }
                inner.writer.write_record(&JournalRecord::Commit).unwrap();

                let after = inner.writer.journal_file_checkpoint();
                self.record_writer_end(after.file_offset);
                after
            };
            // Keep `did_commit_transaction` under `writer_mutex` so `last_end_offset` always
            // advances monotonically even when `commit_holding_commits` runs from `flush_task`
            // (board 11651).
            self.objects.did_commit_transaction(
                transaction,
                &checkpoint_before,
                checkpoint_after.file_offset,
            );
        }

        {
            let mut inner = self.inner.lock();
            // cladefs: a lazy flush task is woken only when it has to flush, arm its timer, or
            // start compacting (see `flush_task`): not for each commit while it compacts.
            let wake = match inner.lazy_flush {
                None => true,
                Some(lazy) => {
                    let journal_bytes = self.objects.last_end_offset()
                        - inner.super_block_header.journal_checkpoint.file_offset;
                    !inner.lazy_flush_armed
                        || inner.writer.flushable_bytes() >= lazy.bytes
                        || inner.position.flush_is_wanted()
                        || (self.should_compact(&inner, journal_bytes) && !inner.compaction_running)
                }
            };
            if wake && let Some(waker) = inner.flush_waker.take() {
                waker.wake();
            }
        }

        Ok(checkpoint_before.file_offset)
    }

    /// This task will flush journal data to the device when there is data that needs flushing, and
    /// trigger compactions when short of journal space.  It will return after the terminate method
    /// has been called, or an error is encountered with either flushing or compaction.
    pub async fn flush_task(self: Arc<Self>) {
        let mut flush_fut = None;
        let mut compact_fut = None;
        let mut flush_error = false;
        // cladefs: when a lazy flush task writes what is buffered regardless.
        let mut flush_timer: Option<std::pin::Pin<Box<fuchsia_async::Timer>>> = None;
        poll_fn(|ctx| {
            loop {
                {
                    let mut inner = self.inner.lock();
                    if flush_fut.is_none() && !flush_error && self.handle.get().is_some() {
                        let flushable = inner.writer.flushable_bytes();
                        if flushable > 0 {
                            let now = match inner.lazy_flush {
                                None => true,
                                Some(lazy) => {
                                    let timer = flush_timer.get_or_insert_with(|| {
                                        Box::pin(fuchsia_async::Timer::new(
                                            std::time::Instant::now() + lazy.delay,
                                        ))
                                    });
                                    inner.lazy_flush_armed = true;
                                    flushable >= lazy.bytes
                                        || inner.position.flush_is_wanted()
                                        || inner.terminate
                                        || timer.poll_unpin(ctx).is_ready()
                                }
                            };
                            if now {
                                flush_timer = None;
                                inner.lazy_flush_armed = false;
                                // cladefs: box in non-inlined frames to keep `flush_task` small on
                                // 16 KiB guest kernel stacks (TASK-26.9).
                                flush_fut = Some(crate::boxed(|| self.flush(flushable)));
                            }
                        }
                    }
                    if inner.terminate && flush_fut.is_none() && compact_fut.is_none() {
                        return Poll::Ready(());
                    }
                    // `journal_bytes` refers to bytes in the journal that haven't yet been flushed
                    // to the layer files. It increases with each transaction and decreases when
                    // compactions complete. The flush task is woken whenever a transaction is
                    // committed and we should see this metric updated regularly.
                    let journal_bytes = self.objects.last_end_offset()
                        - inner.super_block_header.journal_checkpoint.file_offset;
                    // The / 2 is here because after compacting, we cannot reclaim the space until
                    // the _next_ time we flush the device since the super-block is not guaranteed
                    // to persist until then.
                    if compact_fut.is_none()
                        && !inner.terminate
                        && !inner.disable_compactions
                        && !inner.compactions_paused
                        && !inner.compaction_running
                        && self.should_compact(&inner, journal_bytes)
                    {
                        compact_fut = Some(crate::boxed(|| self.compact()));
                        inner.compaction_running = true;
                    }
                    inner.flush_waker = Some(ctx.waker().clone());
                }
                let mut pending = true;
                if let Some(fut) = flush_fut.as_mut() {
                    if let Poll::Ready(result) = fut.poll_unpin(ctx) {
                        if let Err(e) = result {
                            self.inner.lock().terminate(Some(e.context("Flush error")));
                            self.reclaim_event.notify(usize::MAX);
                            flush_error = true;
                        }
                        flush_fut = None;
                        pending = false;
                    }
                }
                if let Some(fut) = compact_fut.as_mut() {
                    if let Poll::Ready(result) = fut.poll_unpin(ctx) {
                        let mut inner = self.inner.lock();
                        if let Err(e) = result {
                            inner.terminate(Some(e.context("Compaction error")));
                        }
                        compact_fut = None;
                        inner.compaction_running = false;
                        self.reclaim_event.notify(usize::MAX);
                        pending = false;
                    }
                }
                if pending {
                    return Poll::Pending;
                }
            }
        })
        .await;
    }

    /// Returns a yielder that can be used for compactions.
    pub fn get_compaction_yielder(&self) -> CompactionYielder<'_> {
        CompactionYielder::new(self)
    }

    async fn flush(&self, mut amount: usize) -> Result<(), Error> {
        let handle = self.handle.get().unwrap();
        // cladefs: on a device whose commits are atomic across all prior writes (`local_cell`,
        // board 7537), `POST_BARRIER` (or the next commit) seals data and journal blocks together,
        // so `PRE_BARRIER` is unnecessary even with `barriers_enabled`. On a non-atomic device,
        // compute one predicate before `record_reaches` and `allocate_buffer` so `record_reaches`
        // and `barrier_on_first_write` never disagree across awaits (TASK-213, board 6011).
        let needs_pre_barrier = !handle.device().atomic_commits();
        let barrier_on_first_write = {
            let mut inner = self.inner.lock();
            let pre = inner.needs_barrier && inner.barriers_enabled && needs_pre_barrier;
            if pre || !needs_pre_barrier {
                inner.needs_barrier = false;
            }
            pre
        };
        // cladefs: record any dropped shared-extent reaches before issuing either a `PRE_BARRIER`
        // (non-atomic device) or a `POST_BARRIER` (including `atomic_commits() == true`), using
        // `record_reaches_for_flush` so a `holding_commits` sync waiting on `flush_task` cannot
        // deadlock `commit_mutex` (TASK-213, board 6011, board 7617, board 11651).
        let mut recorded_reaches = false;
        let allocator = self.objects.allocator();
        let (mut buf, range, mut barrier_after_last_write) = loop {
            let wants_barrier_after = {
                let inner = self.inner.lock();
                let offset = inner.writer.flushed_offset();
                inner.position.wants_barrier_after(&(offset..offset + amount as u64))
            };
            if (barrier_on_first_write || wants_barrier_after)
                && !recorded_reaches
                && !allocator.filesystem_closed()
                && allocator.has_unrecorded_reaches()
            {
                crate::boxed(|| allocator.record_reaches_for_flush()).await?;
                {
                    let _guard = self.writer_mutex.lock();
                    self.pad_to_block()?;
                }
                amount = self.inner.lock().writer.flushable_bytes();
                recorded_reaches = true;
            }
            let mut buf = handle.allocate_buffer(amount).await;
            let mut inner = self.inner.lock();
            let offset = inner.writer.flushed_offset();
            let range = offset..offset + buf.len() as u64;
            // cladefs: as far as a sync flushing the device wants a barrier after.
            let barrier_after = inner.position.wants_barrier_after(&range) || recorded_reaches;
            if barrier_after
                && !recorded_reaches
                && !allocator.filesystem_closed()
                && allocator.has_unrecorded_reaches()
            {
                // A sync arrived during `allocate_buffer` while unrecorded reaches exist; record
                // them before taking flushable bytes so the POST_BARRIER seals no unrecorded reach.
                continue;
            }
            let taken = inner.writer.take_flushable(buf.as_mut());
            debug_assert_eq!(taken, offset);
            break (buf, range, barrier_after);
        };
        let handle = self.handle.get().unwrap();
        if barrier_after_last_write {
            self.objects.allocator().will_flush_device();
            let written = range.end;
            let spans = handle.spans();
            let manager = &self.super_block_manager;
            let update = |root: &mut Root| {
                root.journal_end = root.journal_end.max(written);
                let mut next_spans = spans;
                prune_spans_before(&mut next_spans, root.journal_freed_to);
                root.journal_spans = next_spans;
            };
            barrier_after_last_write = crate::boxed(|| {
                manager.set_root(update, async |root| {
                    Self::flush_with_barrier(
                        handle,
                        range.start,
                        buf.as_mut(),
                        barrier_on_first_write,
                        Some(&root),
                    )
                    .await
                })
            })
            .await?;
        } else {
            crate::boxed(|| {
                handle.write_blocks(range.start, buf.as_mut(), barrier_on_first_write, false, None)
            })
            .await?;
        }

        let mut inner = self.inner.lock();
        if let Some(waker) = inner.sync_waker.take() {
            waker.wake();
        }
        inner.position.flushed(range, barrier_after_last_write);
        Ok(())
    }

    /// cladefs: writes a journal buffer with `POST_BARRIER` (and optional `PRE_BARRIER` + `root`),
    /// retrying without barriers only when `POST_BARRIER` failed after block 0 landed (board 6011,
    /// TASK-26.9).
    async fn flush_with_barrier(
        handle: &JournalExtentHandle,
        start: u64,
        mut buf: storage_device::buffer::MutableBufferRef<'_>,
        barrier_on_first_write: bool,
        root: Option<&[u8]>,
    ) -> Result<bool, Error> {
        let Err(error) = crate::boxed(|| {
            handle.write_blocks(start, buf.reborrow(), barrier_on_first_write, true, root)
        })
        .await
        else {
            return Ok(true);
        };
        warn!(error:?; "The barrier after a journal write failed");
        if barrier_on_first_write {
            let first_len = (BLOCK_SIZE.get() as usize).min(buf.len());
            let mut check = handle.allocate_buffer(first_len).await;
            let pre_barrier_ran = handle.read_aligned(start, check.as_mut()).await.is_ok_and(|n| {
                n == first_len && check.as_ref().to_vec() == buf.subslice(..first_len).to_vec()
            });
            if !pre_barrier_ran {
                return Err(error);
            }
        }
        crate::boxed(|| handle.write_blocks(start, buf, false, false, None)).await?;
        Ok(false)
    }

    /// Whether the journal, holding `journal_bytes` not yet flushed to layer files, should compact:
    /// once half of `reclaim_size`, sooner if the metadata reservation runs low, or when `Root`
    /// has accumulated `COMPACT_SPAN_THRESHOLD` spans across remounts (board 6923). cladefs: or
    /// once flushing the objects it holds updates of takes as much, whatever they hold, as
    /// compacting half of `reclaim_size` (but at least `RESERVED_SPACE`), which hundreds of
    /// stores, each dirtied by a few hundred bytes of it, do long before it fills (board 14243).
    /// The low reservation is what is left over flushing them and compacting the largest store.
    fn should_compact(&self, inner: &Inner, journal_bytes: u64) -> bool {
        let flush_charges = self.objects.flush_charges();
        // What compacting half of `reclaim_size` reserves, which some tests make all of a u64.
        let half_reclaimed =
            reserved_space_from_journal_usage(1).saturating_mul(inner.reclaim_size / 2);
        journal_bytes > inner.reclaim_size / 2
            || self.handle.get().is_some_and(|h| h.span_count() >= COMPACT_SPAN_THRESHOLD)
            || flush_charges > half_reclaimed.max(RESERVED_SPACE)
            || (journal_bytes >= MIN_COMPACTION_BYTES
                && self.objects.metadata_reservation().amount()
                    <= self.objects.max_store_reservation() + flush_charges + METADATA_LOW_MARGIN)
    }

    fn should_force_major_compaction(&self) -> ForceMajor {
        // required_reservation is the total amount of space we have set aside for metadata.  That
        // space is divided into borrowed space (which is used by transactions like file deletions
        // which will eventually, after compaction, give back their space), and
        // metadata_reservation (which is available for use during compaction, both to persist the
        // new layer files and for other transient costs).
        let required_reservation = self.objects.required_reservation();
        // reclaim_size determines the frequency of compaction.  The journal will automatically
        // compact when reclaim_size / 2 <= J <= reclaim_size, and will block most operations once
        // J > reclaim_size, so any given compaction should have no more than reclaim_size bytes.
        let reclaim_size = self.inner.lock().reclaim_size;
        // metadata_reservation = required_reservation - borrowed. cladefs: but for what it holds for
        // flushing the objects the journal holds updates of, whatever they hold (board 14243),
        // which this compaction takes as it flushes them, and is no room for a major one.
        let metadata_reservation = self
            .objects
            .metadata_reservation()
            .amount()
            .saturating_sub(self.objects.flush_charges());
        // max_store_reservation is the greatest amount reserved by any object store.  This
        // corresponds to the minimum amount which is needed to perform a major compaction of that
        // store (and therefore the minimum amount needed to do a major compaction overall, since
        // we compact stores one by one and immediately purge old layer files after compaction,
        // releasing their space back into metaadta_resrvation).
        let max_store_reservation = self.objects.max_store_reservation();

        // Compaction will usually return borrowed space by freeing up space in the journal, but a
        // major or merge compaction is sometimes necessary -- consider if you have one layer file
        // which adds many objects, and another layer file which deletes all of these objects.  If
        // the layers are merged, they cancel out and become very small.  If they are not merged,
        // they do not cancel out and both layers remain large.  The space borrowed to persist the
        // deletion mutations is only given back once they cancel out with the creation mutations.
        //
        // Thus, borrowed space can only be guaranteed to be returned if we eventually
        // major-compact LSM trees.
        //
        // Since major compaction is expensive, we don't want to do it too often, but we also need
        // to make sure we don't wait too long and drain the metadata reservation too much (in
        // particular, it cannot go below `max_store_reservation`, because at that point major
        // compaction might not be possible any more).
        //
        // Thus, we force a major compaction when metadata_reservation <= max_store_reservation -
        // M, for some margin M.  The reason that `reclaim_size` is used as the margin is because
        // `reclaim_size` determines how often we compact, and in theory, the journal is going to
        // be compacted at some point when J < reclaim_size.  This establishes the ideal margin --
        // at this point we are confident that major compaction is still possible, but the next
        // compaction might be another reclaim_size bytes later, which is too late.
        if required_reservation >= reclaim_size
            && metadata_reservation <= max_store_reservation.saturating_sub(reclaim_size)
        {
            ForceMajor::True
        } else {
            ForceMajor::False
        }
    }

    async fn compact(&self) -> Result<(), Error> {
        let began = std::time::Instant::now();
        let _counted = scopeguard::guard((), |()| {
            self.stats.compactions.fetch_add(1, Ordering::Relaxed);
            let ns = began.elapsed().as_nanos() as u64;
            self.stats.compaction_ns.fetch_add(ns, Ordering::Relaxed);
        });
        let force_major = self.should_force_major_compaction();
        debug!("Compaction starting, force_major={force_major:?}");
        // cladefs: box large compaction stages in non-inlined frames so `compact` and `flush_task`
        // stay small on 16 KiB guest kernel stacks (TASK-26.9).
        let earliest_version =
            crate::boxed(|| self.objects.flush(FlushReason::Journal(force_major)))
                .await
                .context("Failed to flush objects")?;
        self.inner.lock().super_block_header.earliest_version = earliest_version;
        crate::boxed(|| self.write_super_block()).await.context("Failed to write superblock")?;
        debug!("Compaction finished");
        Ok(())
    }

    /// cladefs: the number the next sync flushing the device takes as it begins: one done with a
    /// number at least as high began after this was asked, and its flush of the device covers all
    /// that was written, and stored through a DAX window, before (`synced`). Group commit, as
    /// ext4's jbd2 has it: an fsync waiting for another's covered by it needs none of its own.
    pub fn next_sync(&self) -> u64 {
        self.inner.lock().position.next_sync()
    }

    /// cladefs: the number of the latest sync flushing the device that is done (`next_sync`).
    pub fn synced(&self) -> u64 {
        self.inner.lock().position.synced()
    }

    /// cladefs: what it waited for and did ([`JournalStats`]).
    pub fn stats(&self) -> &JournalStats {
        &self.stats
    }

    /// This should generally NOT be called externally. It is public to allow use by FIDL service
    /// fxfs.Debug.
    pub async fn force_compact(&self) -> Result<(), Error> {
        // cladefs: after the flush task's, if one runs, and in its way: two at once wrote super-blocks
        // at once (see `write_super_block`), recording each other's work unfinished: two of them
        // left a crash no super-block to mount from.
        loop {
            debug_assert_not_too_long!({
                let mut inner = self.inner.lock();
                if !inner.compaction_running {
                    inner.compaction_running = true;
                    break;
                }
                self.reclaim_event.listen()
            });
        }
        self.inner.lock().forced_compaction = true;
        scopeguard::defer! {
            let mut inner = self.inner.lock();
            inner.forced_compaction = false;
            inner.compaction_running = false;
            self.reclaim_event.notify(usize::MAX);
        }
        // cladefs: box `compact` in a non-inlined frame so `force_compact` stays small on 16 KiB
        // guest kernel stacks (TASK-26.9).
        crate::boxed(|| self.compact()).await
    }

    pub async fn stop_compactions(&self) {
        loop {
            debug_assert_not_too_long!({
                let mut inner = self.inner.lock();
                inner.disable_compactions = true;
                if !inner.compaction_running {
                    return;
                }
                self.reclaim_event.listen()
            });
        }
    }

    pub async fn pause_compactions(&self) {
        loop {
            debug_assert_not_too_long!({
                let mut inner = self.inner.lock();
                inner.compactions_paused = true;
                if !inner.compaction_running {
                    return;
                }
                self.reclaim_event.listen()
            });
        }
    }

    /// cladefs: whether compactions wait (`pause_compactions`).
    pub(crate) fn compactions_paused(&self) -> bool {
        self.inner.lock().compactions_paused
    }

    pub fn resume_compactions(&self) {
        let mut inner = self.inner.lock();
        inner.compactions_paused = false;
        if let Some(waker) = inner.flush_waker.take() {
            waker.wake();
        }
    }

    /// Terminate all journal activity.
    pub fn terminate(&self) {
        self.inner.lock().terminate(/*reason*/ None);
        self.reclaim_event.notify(usize::MAX);
    }
}

/// cladefs: compactions yield to nothing: upstream's yielder waited for Fuchsia's executor to idle.
pub struct CompactionYielder<'a>(std::marker::PhantomData<&'a Journal>);

impl<'a> CompactionYielder<'a> {
    pub fn new(_journal: &'a Journal) -> Self {
        Self(std::marker::PhantomData)
    }
}

impl crate::lsm_tree::Yielder for CompactionYielder<'_> {
    async fn yield_now(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::{BLOCK_SIZE, JournalOptions};
    use crate::filesystem::{FxFilesystem, FxFilesystemBuilder, OpenFxFilesystem, SyncOptions};
    use crate::fsck::fsck;
    use crate::object_handle::{ObjectHandle, WriteObjectHandle};
    use crate::object_store::directory::Directory;
    use crate::object_store::object_record::{ObjectKey, ObjectValue};
    use crate::object_store::transaction::{Mutation, Options, TRANSACTION_MAX_JOURNAL_USAGE};
    use crate::object_store::volume::root_volume;
    use crate::object_store::{
        HandleOptions, LockKey, MutableNodeAttributes, NewChildStoreOptions, ObjectStore, lock_keys,
    };
    use anyhow::Error;
    use fuchsia_async::{self as fasync, MonotonicDuration};
    use storage_device::DeviceHolder;
    use storage_device::fake_device::FakeDevice;

    const TEST_DEVICE_BLOCK_SIZE: u32 = 512;

    /// cladefs: a mutation's record serialized before the writer's lock is the record
    /// `write_record` writes.
    #[test]
    fn test_serialized_records_are_journal_records() {
        use super::{
            JournalRecord, SerializedRecords, checksums_record_bytes, mutation_record_bytes,
        };
        use crate::checksum::Checksums;
        use crate::object_store::transaction::Mutation;
        use crate::object_store::{ObjectDescriptor, ObjectKey, ObjectValue};
        use crate::serialized_types::Versioned;

        let mutations = [
            Mutation::replace_or_insert_object(
                ObjectKey::child(7, "name"),
                ObjectValue::child(9, ObjectDescriptor::File),
            ),
            Mutation::merge_object(ObjectKey::object(9), ObjectValue::None),
            Mutation::BeginFlush,
        ];
        let mut serialized = SerializedRecords::default();
        let mut expected = Vec::new();
        for (object_id, mutation) in [4, 1 << 40, 5].into_iter().zip(&mutations) {
            serialized.push(object_id, mutation);
            let mut record = Vec::new();
            JournalRecord::Mutation { object_id, mutation: mutation.clone() }
                .serialize_into(&mut record)
                .expect("serialize_into failed");
            assert_eq!(mutation_record_bytes(object_id, mutation), record.len() as u64);
            expected.push(record);
        }
        assert_eq!(serialized.records().map(<[u8]>::to_vec).collect::<Vec<_>>(), expected);

        let range =
            storage_device::ExtentRange::new(storage_device::ExtentId::new(1, 1 << 40), 0..8192);
        let mut record = Vec::new();
        JournalRecord::DataChecksums(range.clone(), Checksums::fletcher(vec![1, 2]), true)
            .serialize_into(&mut record)
            .expect("serialize_into failed");
        assert_eq!(checksums_record_bytes(&range, &[1, 2], true), record.len() as u64);
    }

    // cladefs: a file for `commit_records` to change, with no compaction: what they borrow is more
    // than flushing them could find.
    async fn filesystem_for_records_with(
        device: DeviceHolder,
        options: JournalOptions,
    ) -> (OpenFxFilesystem, u64) {
        let fs = FxFilesystemBuilder::new()
            .format(true)
            .journal_options(options)
            .open(device)
            .await
            .expect("open failed");
        let root_store = fs.root_store();
        let root_directory = Directory::open(&root_store, root_store.root_directory_object_id())
            .await
            .expect("open failed");
        let directory_id = root_store.root_directory_object_id();
        let mut transaction = root_store
            .new_transaction(
                lock_keys![LockKey::object(root_store.store_object_id(), directory_id)],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        let handle = root_directory
            .create_child_file(&mut transaction, "test")
            .await
            .expect("create_child_file failed");
        transaction.commit().await.expect("commit failed");
        let object_id = handle.object_id();
        (fs, object_id)
    }

    fn journal_for_records() -> JournalOptions {
        JournalOptions { reclaim_size: 16 << 20, ..JournalOptions::default() }
    }

    // A commit of about 1500 bytes of records for each of `mutations`, borrowing what more than a
    // transaction reserves, checking that the journal span covers the committed offset.
    async fn commit_records(fs: &OpenFxFilesystem, object_id: u64, round: u32, mutations: usize) {
        try_commit_records(fs, object_id, round, mutations).await.expect("commit failed");
        let journal = fs.journal();
        let size = journal.handle.get().expect("no handle").end_offset();
        let file_offset = journal.inner.lock().writer.journal_file_checkpoint().file_offset;
        assert!(size >= file_offset + BLOCK_SIZE.get(), "at {file_offset} of {size}");
    }

    // `commit_records`' commit, which may fail.
    async fn try_commit_records(
        fs: &OpenFxFilesystem,
        object_id: u64,
        round: u32,
        mutations: usize,
    ) -> Result<u64, Error> {
        let root_store = fs.root_store();
        let store_id = root_store.store_object_id();
        let borrow_metadata_space = mutations as u64 * 1500 > TRANSACTION_MAX_JOURNAL_USAGE;
        let options = Options { borrow_metadata_space, ..Options::default() };
        let mut transaction = root_store
            .new_transaction(lock_keys![LockKey::object(store_id, object_id)], options)
            .await?;
        for i in 0..mutations {
            let name = format!("user.{round}-{i}").into_bytes();
            transaction.add(
                store_id,
                Mutation::insert_object(
                    ObjectKey::extended_attribute(object_id, name),
                    ObjectValue::inline_extended_attribute(vec![round as u8; 1500]),
                ),
            );
        }
        transaction.commit().await
    }

    // Syncs `fs`, and replays and checks what a crash leaves.
    async fn check_replay(fs: OpenFxFilesystem) {
        let synced = SyncOptions { flush_device: true, ..SyncOptions::default() };
        fs.sync(synced).await.expect("sync failed");
        let device = fs.device().snapshot().expect("snapshot failed");
        let replayed = FxFilesystemBuilder::new()
            .journal_options(journal_for_records())
            .open(device)
            .await
            .expect("open failed");
        fsck(replayed.clone()).await.expect("fsck failed");
        // Neither closed: a close compacts what they borrowed, which it can't find room for.
        std::mem::forget((fs, replayed));
    }

    // cladefs: transactions larger than a journal span mint a span big enough to hold them and
    // replay cleanly across span boundaries (TASK-139, board 6923).
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_transactions_larger_than_a_chunk_stay_within_the_journal() {
        let device = DeviceHolder::new(FakeDevice::new(65536, TEST_DEVICE_BLOCK_SIZE));
        let options = JournalOptions { chunk_size: 131_072, ..journal_for_records() };
        let (fs, object_id) = filesystem_for_records_with(device, options).await;
        fs.journal().pause_compactions().await;
        for round in 0..3 {
            commit_records(&fs, object_id, round, 100).await;
        }
        check_replay(fs).await;
    }

    /// cladefs: a mutation of an extent says as much as a record can of what writes it to a layer.
    #[test]
    fn test_mutations_of_extents_are_validated() {
        use super::Journal;
        use crate::checksum::Checksums;
        use crate::object_store::transaction::AllocatorMutation;
        use crate::object_store::{AttributeId, ExtentMode, ExtentValue};
        use storage_device::ExtentId;
        use storage_units::BlockSize;

        let valid = |range, value| {
            let key = ObjectKey::extent(1, AttributeId::DATA, range);
            let mutation = Mutation::merge_object(key, value);
            Journal::validate_mutation(&mutation, BlockSize::SIZE_4KIB)
        };
        assert!(valid(0..4096, ObjectValue::deleted_extent()));
        assert!(!valid(1..4096, ObjectValue::deleted_extent()));
        assert!(!valid(0..0, ObjectValue::deleted_extent()));

        let extent = ExtentId::new(1, 1);
        let sums = |blocks| {
            let mode = ExtentMode::Cow(Checksums::fletcher(vec![7; blocks]));
            ObjectValue::Extent(ExtentValue::new(extent, 0, mode))
        };
        assert!(valid(0..8192, sums(0)));
        assert!(valid(0..8192, sums(2)));
        assert!(!valid(0..8192, sums(3)));
        assert!(!valid(0..8192, sums(4)));

        let allocates = |range| {
            let device_range = storage_device::ExtentRange::new(extent, range);
            let mutation = AllocatorMutation::Allocate { device_range, owner_object_id: 1 };
            Journal::validate_mutation(&Mutation::Allocator(mutation), BlockSize::SIZE_4KIB)
        };
        assert!(allocates(4096..8192));
        assert!(!allocates(1..4096));
        assert!(!allocates(4096..4097));
    }

    /// cladefs: a live transaction that can't be applied, as one freeing what a corrupt allocator
    /// doesn't have, stops the journal rather than panic, and commits nothing after it, or it.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_transaction_that_cannot_be_applied_stops_the_journal() {
        use crate::errors::FxfsError;
        use crate::object_store::transaction::{AllocatorMutation, lock_keys};
        use storage_device::{ExtentId, ExtentRange};

        let device = DeviceHolder::new(FakeDevice::new(8192, 4096));
        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");
        let store = fs.root_store();
        let mut transaction = store
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new_transaction failed");
        let handle = ObjectStore::create_object(&store, &mut transaction, HandleOptions::default())
            .await
            .expect("create_object failed");
        let object_id = handle.object_id();
        transaction.commit().await.expect("commit failed");
        fs.sync(SyncOptions::default()).await.expect("sync failed");

        let allocator = fs.allocator();
        let mut after = store
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new_transaction failed");
        ObjectStore::create_object(&store, &mut after, HandleOptions::default())
            .await
            .expect("create_object failed");
        let mut transaction = store
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new_transaction failed");
        let device_range = ExtentRange::new(ExtentId::new(fs.device().cell(), 1 << 50), 0..4096);
        let owner_object_id = store.store_object_id();
        transaction.add(
            allocator.object_id(),
            Mutation::Allocator(AllocatorMutation::Deallocate { device_range, owner_object_id }),
        );
        let error = transaction.commit().await.expect_err("committed");
        assert!(FxfsError::Inconsistent.matches(&error), "{error:?}");

        let error = after.commit().await.expect_err("committed after the journal stopped");
        assert!(FxfsError::JournalFlushError.matches(&error), "{error:?}");
        store
            .new_transaction(lock_keys![], Options::default())
            .await
            .map(drop)
            .expect_err("a transaction began after the journal stopped");
        drop(handle);

        // What's durable mounts, without the transaction that failed.
        let _ = fs.close().await;
        let device = fs.take_device().await;
        device.reopen(false);
        let fs = FxFilesystem::open(device).await.expect("open failed");
        fsck(fs.clone()).await.expect("fsck failed");
        ObjectStore::open_object(&fs.root_store(), object_id, HandleOptions::default())
            .await
            .expect("what was synced is gone");
        fs.close().await.expect("close failed");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_replay() {
        const TEST_DATA: &[u8] = b"hello";

        let device = DeviceHolder::new(FakeDevice::new(8192, TEST_DEVICE_BLOCK_SIZE));

        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");

        let object_id = {
            let root_store = fs.root_store();
            let root_directory =
                Directory::open(&root_store, root_store.root_directory_object_id())
                    .await
                    .expect("open failed");
            let mut transaction = fs
                .root_store()
                .new_transaction(
                    lock_keys![LockKey::object(
                        root_store.store_object_id(),
                        root_store.root_directory_object_id(),
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
            let mut buf = handle.allocate_buffer(TEST_DATA.len()).await;
            buf.copy_from_slice(TEST_DATA);
            handle.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");
            // As this is the first sync, this will actually trigger a new super-block, but normally
            // this would not be the case.
            fs.sync(SyncOptions::default()).await.expect("sync failed");
            handle.object_id()
        };

        {
            fs.close().await.expect("Close failed");
            let device = fs.take_device().await;
            device.reopen(false);
            let fs = FxFilesystem::open(device).await.expect("open failed");
            let handle =
                ObjectStore::open_object(&fs.root_store(), object_id, HandleOptions::default())
                    .await
                    .expect("open_object failed");
            let mut buf = handle.allocate_buffer(handle.block_size().get() as usize).await;
            assert_eq!(
                handle.read_aligned(0, buf.as_mut()).await.expect("read failed"),
                TEST_DATA.len()
            );
            assert_eq!(buf.subslice(..TEST_DATA.len()).to_vec(), TEST_DATA);
            fsck(fs.clone()).await.expect("fsck failed");
            fs.close().await.expect("Close failed");
        }
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_reset() {
        const TEST_DATA: &[u8] = b"hello";

        let device = DeviceHolder::new(FakeDevice::new(32768, TEST_DEVICE_BLOCK_SIZE));

        let mut object_ids = Vec::new();

        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");
        {
            let root_store = fs.root_store();
            let root_directory =
                Directory::open(&root_store, root_store.root_directory_object_id())
                    .await
                    .expect("open failed");
            let mut transaction = fs
                .root_store()
                .new_transaction(
                    lock_keys![LockKey::object(
                        root_store.store_object_id(),
                        root_store.root_directory_object_id(),
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
            let mut buf = handle.allocate_buffer(TEST_DATA.len()).await;
            buf.copy_from_slice(TEST_DATA);
            handle.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");
            fs.sync(SyncOptions::default()).await.expect("sync failed");
            object_ids.push(handle.object_id());

            // Create a lot of objects but don't sync at the end. This should leave the filesystem
            // with a half finished transaction that cannot be replayed.
            for i in 0..1000 {
                let mut transaction = fs
                    .root_store()
                    .new_transaction(
                        lock_keys![LockKey::object(
                            root_store.store_object_id(),
                            root_store.root_directory_object_id(),
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
                let mut buf = handle.allocate_buffer(TEST_DATA.len()).await;
                buf.copy_from_slice(TEST_DATA);
                handle.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");
                object_ids.push(handle.object_id());
            }
        }
        fs.close().await.expect("fs close failed");
        let device = fs.take_device().await;
        device.reopen(false);
        let fs = FxFilesystem::open(device).await.expect("open failed");
        fsck(fs.clone()).await.expect("fsck failed");
        {
            let root_store = fs.root_store();
            // Check the first two objects which should exist.
            for &object_id in &object_ids[0..1] {
                let handle =
                    ObjectStore::open_object(&root_store, object_id, HandleOptions::default())
                        .await
                        .expect("open_object failed");
                let mut buf = handle.allocate_buffer(handle.block_size().get() as usize).await;
                assert_eq!(
                    handle.read_aligned(0, buf.as_mut()).await.expect("read failed"),
                    TEST_DATA.len()
                );
                let data = buf.subslice(..TEST_DATA.len()).to_vec();
                assert_eq!(data, TEST_DATA);
            }

            // Write one more object and sync.
            let root_directory =
                Directory::open(&root_store, root_store.root_directory_object_id())
                    .await
                    .expect("open failed");
            let mut transaction = fs
                .root_store()
                .new_transaction(
                    lock_keys![LockKey::object(
                        root_store.store_object_id(),
                        root_store.root_directory_object_id(),
                    )],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            let handle = root_directory
                .create_child_file(&mut transaction, "test2")
                .await
                .expect("create_child_file failed");
            transaction.commit().await.expect("commit failed");
            let mut buf = handle.allocate_buffer(TEST_DATA.len()).await;
            buf.copy_from_slice(TEST_DATA);
            handle.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");
            fs.sync(SyncOptions::default()).await.expect("sync failed");
            object_ids.push(handle.object_id());
        }

        fs.close().await.expect("close failed");
        let device = fs.take_device().await;
        device.reopen(false);
        let fs = FxFilesystem::open(device).await.expect("open failed");
        {
            fsck(fs.clone()).await.expect("fsck failed");

            // Check the first two and the last objects.
            for &object_id in object_ids[0..1].iter().chain(object_ids.last().cloned().iter()) {
                let handle =
                    ObjectStore::open_object(&fs.root_store(), object_id, HandleOptions::default())
                        .await
                        .unwrap_or_else(|e| {
                            panic!("open_object failed (object_id: {}): {:?}", object_id, e)
                        });
                let mut buf = handle.allocate_buffer(handle.block_size().get() as usize).await;
                assert_eq!(
                    handle.read_aligned(0, buf.as_mut()).await.expect("read failed"),
                    TEST_DATA.len()
                );
                let data = buf.subslice(..TEST_DATA.len()).to_vec();
                assert_eq!(data, TEST_DATA);
            }
        }
        fs.close().await.expect("close failed");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_discard() {
        let device = {
            let device = DeviceHolder::new(FakeDevice::new(8192, 4096));
            let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");
            let root_volume = root_volume(fs.clone()).await.expect("root_volume failed");

            let store = root_volume
                .new_volume("test", NewChildStoreOptions::default())
                .await
                .expect("new_volume failed");
            let root_directory = Directory::open(&store, store.root_directory_object_id())
                .await
                .expect("open failed");

            // Create enough data so that another journal extent is used.
            let mut i = 0;
            loop {
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
                    .create_child_file(&mut transaction, &format!("a {i}"))
                    .await
                    .expect("create_child_file failed");
                if transaction.commit().await.expect("commit failed") > super::CHUNK_SIZE {
                    break;
                }
                i += 1;
            }

            // Compact and then disable compactions.
            fs.journal().force_compact().await.expect("compact failed");
            fs.journal().stop_compactions().await;

            // Keep going until we need another journal extent.
            let mut i = 0;
            loop {
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
                    .create_child_file(&mut transaction, &format!("b {i}"))
                    .await
                    .expect("create_child_file failed");
                if transaction.commit().await.expect("commit failed") > 2 * super::CHUNK_SIZE {
                    break;
                }
                i += 1;
            }

            // Allow the journal to flush, but we don't want to sync.
            fasync::Timer::new(MonotonicDuration::from_millis(10)).await;
            // Because we're not gracefully closing the filesystem, a Discard record will be
            // emitted.
            fs.device().snapshot().expect("snapshot failed")
        };

        let fs = FxFilesystem::open(device).await.expect("open failed");

        {
            let root_volume = root_volume(fs.clone()).await.expect("root_volume failed");

            let store = root_volume.volume("test").await.expect("volume failed");

            let root_directory = Directory::open(&store, store.root_directory_object_id())
                .await
                .expect("open failed");

            // Write one more transaction.
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
                .create_child_file(&mut transaction, &format!("d"))
                .await
                .expect("create_child_file failed");
            transaction.commit().await.expect("commit failed");
        }

        fs.close().await.expect("close failed");
        let device = fs.take_device().await;
        device.reopen(false);

        let fs = FxFilesystem::open(device).await.expect("open failed");
        fsck(fs.clone()).await.expect("fsck failed");
        fs.close().await.expect("close failed");
    }

    // cladefs: transactions that borrow metadata space, such as unlinks, each followed by a sync,
    // whose padding the next transaction's journal usage includes, borrow faster than the journal
    // grows. With a journal larger than upstream's, they drained the metadata reservation before
    // the journal filled enough to compact (PostgreSQL's initdb failed with ENOSPC in the guest):
    // it compacts sooner, on a thread of its own, as the guest's does. The unlinks skip the journal
    // checks so that they borrow, as a transaction that may borrow reserves if it can.
    #[fuchsia_async::run(2, test)]
    async fn test_borrowing_compacts_before_the_metadata_reservation_runs_out() {
        use crate::object_store::directory::replace_child;

        let device = DeviceHolder::new(FakeDevice::new(1 << 21, 4096));
        let fs = FxFilesystemBuilder::new()
            .journal_options(super::JournalOptions { reclaim_size: 8 << 20, ..Default::default() })
            .format(true)
            .open(device)
            .await
            .expect("open failed");
        let store = fs.root_store();
        let dir =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");
        let lock = || {
            lock_keys![LockKey::object(store.store_object_id(), store.root_directory_object_id())]
        };
        for i in 0..2000 {
            let name = format!("temp-{i}");
            let mut transaction = store
                .new_transaction(lock(), Options::default())
                .await
                .expect("new_transaction failed");
            let file = dir.create_child_file(&mut transaction, &name).await.expect("create failed");
            transaction.commit().await.expect("commit failed");
            fs.sync(SyncOptions { flush_device: true, ..Default::default() })
                .await
                .expect("sync failed");
            let mut keys = lock();
            keys.push(LockKey::object(store.store_object_id(), file.object_id()));
            let options = Options {
                borrow_metadata_space: true,
                skip_journal_checks: true,
                ..Default::default()
            };
            let mut transaction =
                store.new_transaction(keys, options).await.expect("new_transaction failed");
            replace_child(&mut transaction, None, (&dir, &name)).await.expect("unlink failed");
            transaction.commit().await.unwrap_or_else(|e| panic!("unlink {i}: {e:?}"));
            fs.sync(SyncOptions { flush_device: true, ..Default::default() })
                .await
                .unwrap_or_else(|e| panic!("sync after unlink {i}: {e:?}"));
        }
        fs.close().await.expect("close failed");
    }

    // cladefs: a sync pads the journal to its block, which the next commit's transaction pays for,
    // borrowing it if it is more than a little. The guest's Fxfs without DAX failed PostgreSQL's
    // initdb, whose last step fsyncs each of its files in turn: each sync's padding, borrowed four
    // times over, drained the metadata reservation, and its large journal compacted too late for
    // its compaction to find room (TASK-26.6.1). Transactions that borrow nothing, each followed by
    // a sync, leave the metadata reservation no smaller.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_syncs_padding_is_reserved_not_borrowed() {
        let device = DeviceHolder::new(FakeDevice::new(1 << 21, 4096));
        let fs = FxFilesystemBuilder::new()
            .journal_options(super::JournalOptions { reclaim_size: 8 << 20, ..Default::default() })
            .format(true)
            .open(device)
            .await
            .expect("open failed");
        let root_volume = root_volume(fs.clone()).await.expect("root_volume failed");
        let store = root_volume
            .new_volume("default", NewChildStoreOptions::default())
            .await
            .expect("new_volume failed");
        let dir =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");
        let mut transaction = store
            .new_transaction(
                lock_keys![LockKey::object(store.store_object_id(), dir.object_id())],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        let file = dir.create_child_file(&mut transaction, "file").await.expect("create failed");
        transaction.commit().await.expect("commit failed");
        fs.sync(SyncOptions { flush_device: true, ..Default::default() })
            .await
            .expect("sync failed");
        let reserved = fs.object_manager().metadata_reservation().amount();
        for i in 0..100u64 {
            let mut transaction = store
                .new_transaction(
                    lock_keys![LockKey::object(store.store_object_id(), file.object_id())],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            let times = MutableNodeAttributes { modification_time: Some(i), ..Default::default() };
            file.update_attributes(&mut transaction, Some(&times), None)
                .await
                .expect("update_attributes failed");
            transaction.commit().await.expect("commit failed");
            fs.sync(SyncOptions { flush_device: true, ..Default::default() })
                .await
                .expect("sync failed");
        }
        // Extending the journal borrows, as upstream's does, until compacting it repays: but the
        // syncs' padding is paid for, and the metadata reservation compactions draw on has not
        // shrunk.
        let now = fs.object_manager().metadata_reservation().amount();
        assert!(now >= reserved, "the metadata reservation shrank from {reserved} to {now}");
        fs.close().await.expect("close failed");
    }

    // cladefs: a mount's replay finds the metadata space borrowed as the filesystem had it when it
    // stopped. A transaction that may borrow but reserves what it can (`Borrowing`), such as an
    // unlink or a tombstone, borrowed nothing live, but replay took it as having borrowed all its
    // journal usage: each mount whose replay ended past one borrowed it again, which the next
    // super-block kept and no compaction repaid. A guest store through 37 rounds of crashes and
    // pgbench mounted with 10 MB borrowed, more than its reservation holds, and its first
    // compaction found no room (TASK-218).
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_a_mount_borrows_what_the_filesystem_had_borrowed() {
        let mut device = DeviceHolder::new(FakeDevice::new(1 << 16, 4096));
        let fs = FxFilesystemBuilder::new().format(true).open(device).await.expect("open failed");
        let volumes = root_volume(fs.clone()).await.expect("root_volume failed");
        let store = volumes
            .new_volume("default", NewChildStoreOptions::default())
            .await
            .expect("new_volume failed");
        let dir =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");
        let mut transaction = store
            .new_transaction(
                lock_keys![LockKey::object(store.store_object_id(), dir.object_id())],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        let file = dir.create_child_file(&mut transaction, "file").await.expect("create failed");
        transaction.commit().await.expect("commit failed");
        let file_id = file.object_id();
        drop((file, dir, store, volumes));
        let mut fs = fs;
        for round in 0..8u64 {
            let volumes = root_volume(fs.clone()).await.expect("root_volume failed");
            let store = volumes.volume("default").await.expect("volume");
            let file = ObjectStore::open_object(&store, file_id, HandleOptions::default())
                .await
                .expect("open_object failed");
            let options = Options { borrow_metadata_space: true, ..Default::default() };
            let mut transaction = store
                .new_transaction(
                    lock_keys![LockKey::object(store.store_object_id(), file_id)],
                    options,
                )
                .await
                .expect("new_transaction failed");
            let times =
                MutableNodeAttributes { modification_time: Some(round), ..Default::default() };
            file.update_attributes(&mut transaction, Some(&times), None)
                .await
                .expect("update_attributes failed");
            transaction.commit().await.expect("commit failed");
            drop((file, store, volumes));
            let borrowed = fs.object_manager().borrowed_metadata_space();
            fs.close().await.expect("close failed");
            device = fs.take_device().await;
            device.reopen(false);
            fs = FxFilesystemBuilder::new().open(device).await.expect("open failed");
            let replayed = fs.object_manager().borrowed_as_replayed();
            assert_eq!(replayed, borrowed, "round {round}: borrowed as replayed, and as it was");
        }
        fs.close().await.expect("close failed");
    }

    // cladefs: a transaction that may borrow metadata space, such as an unlink, or the graveyard's
    // tombstone of what it unlinked, reserves it with the allocator while the filesystem has room:
    // borrowed, it drains the metadata reservation until the journal compacts.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_borrowing_transactions_reserve_while_there_is_room() {
        use crate::object_store::directory::replace_child;

        let device = DeviceHolder::new(FakeDevice::new(1 << 21, 4096));
        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");
        let store = fs.root_store();
        let dir =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");
        let lock = || {
            lock_keys![LockKey::object(store.store_object_id(), store.root_directory_object_id())]
        };
        let mut transaction = store
            .new_transaction(lock(), Options::default())
            .await
            .expect("new_transaction failed");
        let file = dir.create_child_file(&mut transaction, "file").await.expect("create failed");
        transaction.commit().await.expect("commit failed");
        let borrowed = fs.object_manager().borrowed_metadata_space();

        let mut keys = lock();
        keys.push(LockKey::object(store.store_object_id(), file.object_id()));
        let options = Options { borrow_metadata_space: true, ..Default::default() };
        let mut transaction =
            store.new_transaction(keys, options).await.expect("new_transaction failed");
        replace_child(&mut transaction, None, (&dir, "file")).await.expect("unlink failed");
        transaction.commit().await.expect("commit failed");
        assert_eq!(fs.object_manager().borrowed_metadata_space(), borrowed);
        fs.close().await.expect("close failed");
    }

    /// cladefs: a mount reads the journal it replays ahead, in windows of blocks it reads at once,
    /// each an extent of its own: replaying a journal of hundreds of blocks waits for a round trip
    /// to the device per window of them, not per block (TASK-92.18).
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_mount_reads_the_journal_ahead() {
        use std::sync::Arc;
        use storage_device::fake_device::RoundTrips;

        let trips = Arc::new(RoundTrips::default());
        let device = DeviceHolder::new(
            FakeDevice::new(1 << 16, 4096).calm().with_round_trips(trips.clone()),
        );
        let options = || super::JournalOptions { reclaim_size: 8 << 20, ..Default::default() };
        let fs = FxFilesystemBuilder::new()
            .journal_options(options())
            .format(true)
            .open(device)
            .await
            .expect("open failed");
        let replay_from = fs.super_block_header().journal_checkpoint.file_offset;
        let store = fs.root_store();
        let dir =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");
        let mut end = replay_from;
        let mut files = 0;
        while end < replay_from + (1 << 20) {
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
            dir.create_child_file(&mut transaction, &format!("file {files}"))
                .await
                .expect("create_child_file failed");
            end = transaction.commit().await.expect("commit failed");
            files += 1;
        }
        // Nothing compacted meanwhile: the mount replays all of it.
        assert_eq!(fs.super_block_header().journal_checkpoint.file_offset, replay_from);
        fs.close().await.expect("close failed");
        let device = fs.take_device().await;
        device.reopen(false);

        let (reads, before) = (trips.reads.count(), trips.reads.trips());
        let fs = FxFilesystemBuilder::new()
            .journal_options(options())
            .open(device)
            .await
            .expect("open failed");
        let (reads, trips) = (trips.reads.count() - reads, trips.reads.trips() - before);
        let blocks = (end - replay_from) / 4096;
        // Replay reads the journal's 256 blocks, to the first not written past them, in 13 windows
        // (of 1, 2, 4, 8 and 16 blocks, then 32): the rest of the mount (the single super-block and
        // the layer files, with no renew_tail probe) takes 2 round trips more.
        assert_eq!(
            (blocks, trips),
            (256, 15),
            "replaying {blocks} blocks, the mount read {reads} times, in {trips} round trips"
        );
        let store = fs.root_store();
        let dir =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");
        assert_eq!(
            dir.lookup(&format!("file {}", files - 1)).await.expect("lookup failed").is_some(),
            true
        );
        fs.close().await.expect("close failed");
    }

    // cladefs: a lazy flush task leaves transactions in memory until something waits for them, or
    // its delay passes.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_lazy_flush() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        use storage_device::fake_device::Op;

        let writes = Arc::new(AtomicUsize::new(0));
        let mut device = FakeDevice::new(16384, TEST_DEVICE_BLOCK_SIZE);
        let counted = writes.clone();
        device.set_op_callback(move |op| {
            if let Op::Write = op {
                counted.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        });
        let lazy_flush = super::LazyFlush { bytes: 1 << 20, delay: Duration::from_millis(300) };
        let fs = FxFilesystemBuilder::new()
            .journal_options(super::JournalOptions {
                lazy_flush: Some(lazy_flush),
                ..Default::default()
            })
            .format(true)
            .open(DeviceHolder::new(device))
            .await
            .expect("open failed");
        let root_store = fs.root_store();
        let root_directory = Directory::open(&root_store, root_store.root_directory_object_id())
            .await
            .expect("open failed");
        let create = |name: String| {
            let (fs, root_store, root_directory) = (&fs, &root_store, &root_directory);
            async move {
                let mut transaction = fs
                    .root_store()
                    .new_transaction(
                        lock_keys![LockKey::object(
                            root_store.store_object_id(),
                            root_store.root_directory_object_id(),
                        )],
                        Options::default(),
                    )
                    .await
                    .expect("new_transaction failed");
                root_directory
                    .create_child_file(&mut transaction, &name)
                    .await
                    .expect("create_child_file failed");
                transaction.commit().await.expect("commit failed");
            }
        };

        // Transactions stay in memory, more blocks of them than one.
        fs.sync(SyncOptions { flush_device: true, ..Default::default() }).await.expect("sync");
        let synced = writes.load(Ordering::Relaxed);
        for i in 0..100 {
            create(format!("in-memory-{i}")).await;
        }
        assert_eq!(writes.load(Ordering::Relaxed), synced);

        // A sync writes them at once.
        fs.sync(SyncOptions { flush_device: true, ..Default::default() }).await.expect("sync");
        let synced_again = writes.load(Ordering::Relaxed);
        assert_ne!(synced_again, synced, "the sync wrote nothing");

        // Once the delay passes, the flush task writes what is buffered.
        for i in 0..100 {
            create(format!("delayed-{i}")).await;
        }
        assert_eq!(writes.load(Ordering::Relaxed), synced_again);
        fasync::Timer::new(MonotonicDuration::from_millis(1000)).await;
        assert_ne!(writes.load(Ordering::Relaxed), synced_again, "the flush task wrote nothing");

        fs.close().await.expect("close failed");
    }

    /// cladefs: a sync flushes the device once: the padding it writes to finish the journal's
    /// block needs no flush of its own, for the next sync with nothing new.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_sync_with_nothing_new_flushes_nothing() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use storage_device::fake_device::Op;

        let flushes = Arc::new(AtomicUsize::new(0));
        let mut device = FakeDevice::new(16384, TEST_DEVICE_BLOCK_SIZE);
        let counted = flushes.clone();
        device.set_op_callback(move |op| {
            if let Op::Flush = op {
                counted.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        });
        let fs =
            FxFilesystem::new_empty(DeviceHolder::new(device)).await.expect("new_empty failed");
        let root_store = fs.root_store();
        let root_directory = Directory::open(&root_store, root_store.root_directory_object_id())
            .await
            .expect("open failed");
        let mut transaction = root_store
            .new_transaction(
                lock_keys![LockKey::object(
                    root_store.store_object_id(),
                    root_store.root_directory_object_id(),
                )],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        root_directory.create_child_file(&mut transaction, "file").await.expect("create failed");
        transaction.commit().await.expect("commit failed");

        let sync = || fs.sync(SyncOptions { flush_device: true, ..Default::default() });
        sync().await.expect("sync");
        let synced = flushes.load(Ordering::Relaxed);
        sync().await.expect("sync");
        assert_eq!(flushes.load(Ordering::Relaxed), synced, "the second sync flushed the device");
        fs.close().await.expect("close failed");
    }

    /// cladefs: a sync's flush of the device is the barrier after its journal's last write, one
    /// request, where each is a round trip to the guest's host.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_sync_barrier_follows_the_journal_write() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use storage_device::fake_device::Op;

        let (flushes, post_barriers) =
            (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let mut device = FakeDevice::new(16384, TEST_DEVICE_BLOCK_SIZE);
        let counted = (flushes.clone(), post_barriers.clone());
        device.set_op_callback(move |op| {
            match op {
                Op::Flush => counted.0.fetch_add(1, Ordering::Relaxed),
                Op::PostBarrier => counted.1.fetch_add(1, Ordering::Relaxed),
                _ => 0,
            };
            Ok(())
        });
        let fs =
            FxFilesystem::new_empty(DeviceHolder::new(device)).await.expect("new_empty failed");
        let root_store = fs.root_store();
        let root_directory = Directory::open(&root_store, root_store.root_directory_object_id())
            .await
            .expect("open failed");
        let counts = || (flushes.load(Ordering::Relaxed), post_barriers.load(Ordering::Relaxed));
        for i in 0..3 {
            let mut transaction = root_store
                .new_transaction(
                    lock_keys![LockKey::object(
                        root_store.store_object_id(),
                        root_store.root_directory_object_id(),
                    )],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            root_directory
                .create_child_file(&mut transaction, &format!("file-{i}"))
                .await
                .expect("create failed");
            transaction.commit().await.expect("commit failed");
            let (flushed, barriers) = counts();
            fs.sync(SyncOptions { flush_device: true, ..Default::default() }).await.expect("sync");
            assert_eq!(counts(), (flushed, barriers + 1), "sync {i}");
        }
        fs.close().await.expect("close failed");
    }

    /// cladefs: a barrier after the journal's write that fails, as a full store's commit does,
    /// fails the sync that wanted it, not the journal: the next sync succeeds.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_failed_sync_barrier_leaves_the_journal_open() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use storage_device::fake_device::Op;

        let full = Arc::new(AtomicBool::new(false));
        let mut device = FakeDevice::new(16384, TEST_DEVICE_BLOCK_SIZE);
        let failing = full.clone();
        device.set_op_callback(move |op| {
            if matches!(op, Op::Flush | Op::PostBarrier) && failing.load(Ordering::Relaxed) {
                anyhow::bail!(crate::errors::FxfsError::NoSpace);
            }
            Ok(())
        });
        let fs =
            FxFilesystem::new_empty(DeviceHolder::new(device)).await.expect("new_empty failed");
        let root_store = fs.root_store();
        let root_directory = Directory::open(&root_store, root_store.root_directory_object_id())
            .await
            .expect("open failed");
        let create = |name: &'static str| {
            let (root_store, root_directory) = (&root_store, &root_directory);
            async move {
                let mut transaction = root_store
                    .new_transaction(
                        lock_keys![LockKey::object(
                            root_store.store_object_id(),
                            root_store.root_directory_object_id(),
                        )],
                        Options::default(),
                    )
                    .await
                    .expect("new_transaction failed");
                root_directory.create_child_file(&mut transaction, name).await.expect("create");
                transaction.commit().await.expect("commit failed");
            }
        };
        let sync = || fs.sync(SyncOptions { flush_device: true, ..Default::default() });

        create("full").await;
        full.store(true, Ordering::Relaxed);
        sync().await.expect_err("the sync's barrier failed");
        full.store(false, Ordering::Relaxed);
        create("room").await;
        sync().await.expect("sync after the store has room");
        fs.close().await.expect("close failed");
    }

    /// cladefs: damage ending the journal before where a barrier last found it written is refused,
    /// as it would lose what syncs acknowledged unsaid; past there, the journal ends where it ends,
    /// as a crash may tear it anywhere (TASK-153.13).
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_a_journal_ending_before_its_synced_end_is_refused() {
        use super::BLOCK_SIZE;
        use crate::errors::FxfsError;
        use storage_device::fake_device::Crashes;

        let crashes = Crashes::new(0, u32::MAX);
        let device = FakeDevice::new(16384, TEST_DEVICE_BLOCK_SIZE).record_crashes(crashes.clone());
        let fs =
            FxFilesystem::new_empty(DeviceHolder::new(device)).await.expect("new_empty failed");
        let root_store = fs.root_store();
        let root_directory = Directory::open(&root_store, root_store.root_directory_object_id())
            .await
            .expect("open failed");
        let create = async |name: &str| {
            let mut transaction = root_store
                .new_transaction(
                    lock_keys![LockKey::object(
                        root_store.store_object_id(),
                        root_store.root_directory_object_id(),
                    )],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            root_directory.create_child_file(&mut transaction, name).await.expect("create");
            transaction.commit().await.expect("commit failed");
        };
        let synced: Vec<String> = (0..4).map(|i| format!("synced-{i}")).collect();
        let unsynced: Vec<String> = (0..4).map(|i| format!("unsynced-{i}")).collect();
        for name in &synced {
            create(name).await;
            fs.sync(SyncOptions { flush_device: true, ..Default::default() }).await.expect("sync");
        }
        let journal = fs.journal();
        let synced_end = journal.super_block_manager.root().await.journal_end;
        let super_block_offset = journal.super_block_header().super_block_journal_file_offset;
        // Written, with no barrier after.
        for name in &unsynced {
            create(name).await;
        }
        let (checkpoint, _) = journal.sync(SyncOptions::default()).await.expect("sync").unwrap();
        journal.wait_for_flush(checkpoint.file_offset).await.expect("written");
        let written = journal.inner.lock().position.flushed_offset();
        assert!(super_block_offset < synced_end - BLOCK_SIZE, "{super_block_offset} {synced_end}");
        assert!(synced_end < written, "{synced_end} {written}");
        let journal_handle = journal.handle.get().unwrap().clone();
        let block_at = |offset: u64| {
            let extents = journal_handle.extents_in(offset..offset + BLOCK_SIZE.get());
            let extent = *extents.first().expect("a journal block");
            (extent, 0usize)
        };
        // As a crash leaves it, losing nothing.
        crashes.mark(0);
        crashes.last();
        fs.device().flush().await.expect("flush failed");
        fs.close().await.expect("close failed");
        let (_, image) = crashes.take().next().expect("what the device held");

        // A flip in the last block a sync wrote, or in the first after.
        let flipped = |offset: u64| {
            let device = image.recovered(0);
            let (extent, at) = block_at(offset);
            device.corrupt(|extents, _| extents.get_mut(&extent).expect("the block")[at + 8] ^= 1);
            DeviceHolder::new(device)
        };
        let error = FxFilesystem::open(flipped(synced_end - BLOCK_SIZE))
            .await
            .map(drop)
            .expect_err("a journal damaged before its synced end mounted");
        assert!(FxfsError::Inconsistent.matches(&error), "{error:?}");

        let fs = FxFilesystem::open(flipped(synced_end)).await.expect("open failed");
        let root_store = fs.root_store();
        let root_directory = Directory::open(&root_store, root_store.root_directory_object_id())
            .await
            .expect("open failed");
        for name in &synced {
            assert!(root_directory.lookup(name).await.expect("lookup").is_some(), "{name}");
        }
        for name in &unsynced {
            assert!(root_directory.lookup(name).await.expect("lookup").is_none(), "{name}");
        }
        fsck(fs.clone()).await.expect("fsck failed");
        fs.close().await.expect("close failed");
    }
}

// cladefs: plain functions, which the targets of crates/fxfs/fuzz call, in place of Fuchsia's `fuzz`
// macro, on the image of `crate::testing::fuzz`: Fxfs's blocks are 4 KiB here. What mounts is
// checked and read too.
#[cfg(fuzz)]
pub mod fuzz {
    use crate::filesystem::{FxFilesystem, OpenFxFilesystem};
    use crate::object_store::transaction::{Options, lock_keys};
    use crate::object_store::{HandleOptions, ObjectStore};
    use crate::testing::fuzz::{image, mounts_without_panicking, run};
    use std::io::Write;
    use storage_device::DeviceHolder;

    /// The image, mounted, one transaction committed: which starts the flush task, and writes the
    /// reset that replay looks for.
    async fn mounted() -> OpenFxFilesystem {
        let fs = FxFilesystem::open(DeviceHolder::new(image())).await.expect("open failed");
        let store = fs.root_store();
        let mut transaction = store
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new_transaction failed");
        ObjectStore::create_object(&store, &mut transaction, HandleOptions::default())
            .await
            .expect("create_object failed");
        transaction.commit().await.expect("commit failed");
        fs
    }

    pub fn fuzz_journal_bytes(input: Vec<u8>) {
        run(async move {
            let fs = mounted().await;
            fs.journal().inner.lock().writer.write_all(&input).expect("write failed");
            fs.close().await.expect("close failed");
            let device = fs.take_device().await;
            device.reopen(false);
            mounts_without_panicking(device).await;
        });
    }

    pub fn fuzz_journal(input: Vec<super::JournalRecord>) {
        run(async move {
            let fs = mounted().await;
            {
                let mut inner = fs.journal().inner.lock();
                for record in &input {
                    let _ = inner.writer.write_record(record);
                }
            }
            fs.close().await.expect("close failed");
            let device = fs.take_device().await;
            device.reopen(false);
            mounts_without_panicking(device).await;
        });
    }
}
