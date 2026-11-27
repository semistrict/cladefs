// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Fuchsia's `src/storage/fxfs/src/object_store/journal/super_block.rs`. cladefs: replaces A/B
// super-blocks (`SuperBlockInstance::{A, B}`, dual object IDs, generation tie-breaking, and
// `SuperBlockRecord::Extent` chains) with a single atomic super-block extent named directly by
// `Root::super_block` and swapped atomically by `device.commit_root()` (board 6926). `Root` also
// stores the contiguous position-derived `JournalSpan`s, `journal_freed_to`, `retired_super_block`,
// and `journal_end` (boards 6923, 6926; TASK-153.13). Superseded super-block and journal block
// extents are freed only after `commit_root`, and dropped from `Root` only after a barrier seals
// their frees.

//! A single atomic super-block referenced directly from `Root::super_block`.
//!
//! Immediately following the serialized `SuperBlockHeader` structure below is a stream of
//! serialized `SuperBlockRecord::ObjectItem` records that are replayed into the root parent
//! `ObjectStore`, ending with `SuperBlockRecord::End`. The root parent object store exists
//! entirely in RAM until serialized back into a fresh write-once super-block extent.

use crate::checksum::Checksum;
use crate::errors::FxfsError;
use crate::filesystem::{ApplyContext, ApplyMode, FxFilesystem, JournalingObject};
use crate::log::*;
use crate::lsm_tree::types::LayerIterator;
use crate::lsm_tree::{LSMTree, LayerSet, Query};
use crate::object_handle::INVALID_OBJECT_ID;
use crate::object_store::journal::bootstrap_handle::{
    ExtentObjectHandle, JournalExtentHandle, prune_spans_before,
};
use crate::object_store::journal::reader::{JournalReader, ReadResult};
use crate::object_store::journal::writer::JournalWriter;
use crate::object_store::journal::{BLOCK_SIZE, Journal, JournalCheckpoint, JournalCheckpointV32};
use crate::object_store::object_record::ObjectItemV66;
use crate::object_store::transaction::AssocObj;
use crate::object_store::tree::MajorCompactable;
use crate::object_store::{Mutation, ObjectKey, ObjectStore, ObjectValue};
use crate::serialized_types::{
    EARLIEST_SUPPORTED_VERSION, LATEST_VERSION, Version, Versioned, VersionedLatest,
};
use anyhow::{Context, Error, anyhow, bail, ensure};
use fprint::TypeFingerprint;
use fuchsia_sync::Mutex;
use futures::FutureExt;
use rustc_hash::FxHashMap as HashMap;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::io::{Read, Write};
use std::sync::Arc;
use storage_device::{Device, ExtentId, ExtentRange};
use storage_units::BlockSize;
use uuid::Uuid;

/// Each super-block is at least one block.
pub(crate) const MIN_SUPER_BLOCK_SIZE: u64 = 4096;

/// Maximum single write chunk when writing a super-block extent.
/// cladefs: past any journal's end: a super-block or span saying otherwise is refused.
pub(crate) const MAX_JOURNAL_OFFSET: u64 = 1 << 62;

/// Maximum blocks allowed in a single `JournalSpan` (16 MiB), bounding replay's work and memory
/// against hostile `Root` or `JournalRecord::JournalSpan` inputs (board 6923).
pub(crate) const MAX_SPAN_BLOCKS: u64 = 4096;

/// Maximum extent sequence accepted from untrusted `Root` or `JournalSpan` inputs, keeping
/// headroom below `u64::MAX` so writable mount never overflows when minting new extents.
pub(crate) const MAX_EXTENT_SEQUENCE: u64 = u64::MAX - (1 << 20);

/// Maximum spans allowed in `Root` (keeps encoded `Root` strictly within `storage_device::MAX_ROOT`
/// = 4096 bytes).
pub(crate) const MAX_ROOT_SPANS: usize = 64;

/// Maximum retired super-block extents tracked in `Root`.
const MAX_ROOT_RETIRED: usize = 16;

/// All super-blocks start with the magic bytes "FxfsSupr".
const SUPER_BLOCK_MAGIC: &[u8; 8] = b"FxfsSupr";

/// cladefs: a contiguous run of journal blocks, each a 4 KiB extent of its own in `cell`, whose
/// sequence number for aligned stream offset `off` is `base_sequence + (off - start_offset) / 4096`
/// (board 6923).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize, TypeFingerprint)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub struct JournalSpan {
    /// Block-aligned journal stream byte offset where this span begins.
    pub start_offset: u64,
    /// Number of `BLOCK_SIZE` blocks in this span (non-zero in any valid span).
    pub block_count: u64,
    /// Cell holding these block extents.
    pub cell: u64,
    /// Sequence number of the first block (`start_offset`).
    pub base_sequence: u64,
}

impl JournalSpan {
    /// Validates an untrusted span decoded from `Root` or a journal record.
    pub fn validate(&self) -> Result<(), Error> {
        ensure!(
            BLOCK_SIZE.is_aligned(self.start_offset)
                && self.block_count > 0
                && self.block_count <= MAX_SPAN_BLOCKS,
            anyhow!(FxfsError::Inconsistent).context(format!("Invalid journal span {self:?}"))
        );
        let bytes = self.block_count.checked_mul(BLOCK_SIZE.get()).ok_or_else(|| {
            anyhow!(FxfsError::Inconsistent).context("Journal span byte overflow")
        })?;
        let end_offset = self
            .start_offset
            .checked_add(bytes)
            .ok_or_else(|| anyhow!(FxfsError::Inconsistent).context("Journal span end overflow"))?;
        ensure!(
            end_offset <= MAX_JOURNAL_OFFSET,
            anyhow!(FxfsError::Inconsistent).context("Journal span exceeds MAX_JOURNAL_OFFSET")
        );
        // Keep every span well below `u64::MAX` so `cladefsd` never sees sequence `u64::MAX` and
        // writable mount never overflows when minting fresh spans past it.
        let end_seq = self.base_sequence.checked_add(self.block_count).ok_or_else(|| {
            anyhow!(FxfsError::Inconsistent).context("Journal span sequence overflow")
        })?;
        ensure!(
            end_seq <= MAX_EXTENT_SEQUENCE,
            anyhow!(FxfsError::Inconsistent)
                .context("Journal span sequence exceeds MAX_EXTENT_SEQUENCE")
        );
        Ok(())
    }

    /// Stream byte offset one past the end of this span.
    pub fn end_offset(&self) -> u64 {
        self.start_offset + self.block_count * BLOCK_SIZE.get()
    }

    /// Sequence number one past the last block of this span (invariant under front-trimming).
    pub fn end_sequence(&self) -> u64 {
        self.base_sequence + self.block_count
    }

    /// Resolves a block-aligned stream byte offset inside this span to its `ExtentId`.
    pub fn extent_at(&self, offset: u64) -> Option<ExtentId> {
        let end = self.end_offset();
        if offset < self.start_offset || offset >= end {
            return None;
        }
        let block = (offset - self.start_offset) / BLOCK_SIZE.get();
        Some(ExtentId::new(self.cell, self.base_sequence + block))
    }
}

/// cladefs: the extent store's root (backlog doc-4; boards 6923, 6926), which atomically names the
/// active super-block extent, any retired super-block extents whose frees are not yet sealed, the
/// active position-derived journal spans, and the synced journal end.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub struct Root {
    /// The active super-block extent (`0..len`, block-aligned).
    pub super_block: Option<ExtentRange>,
    /// Previous super-block extents whose frees have not yet been sealed by a device flush.
    pub retired_super_block: Vec<ExtentId>,
    /// Contiguous journal spans covering from `journal_freed_to` through the current write span.
    pub journal_spans: Vec<JournalSpan>,
    /// Block-aligned journal offset up to which superseded journal block frees are sealed.
    pub journal_freed_to: u64,
    /// How far the journal had been written before the barrier that set this root, which a crash
    /// keeps with everything before it (doc-4): a replay that ends short of it ended at damage, not
    /// at the journal's end, losing transactions a sync acknowledged, and refuses (TASK-153.13).
    /// Past it, the journal may end anywhere, torn by a crash.
    pub journal_end: u64,
}

/// A V64 root starts with the magic bytes "FxfsRt02" (old "FxfsRoot" is refused outright).
const ROOT_MAGIC: &[u8; 8] = b"FxfsRt02";

impl Root {
    /// Reads and validates the root of `device`, which is empty until formatted.
    pub async fn read(device: &dyn Device) -> Result<Self, Error> {
        use bincode::Options as _;
        let bytes = device.root().await.context("Failed to read the root")?;
        if bytes.is_empty() {
            return Ok(Self::default());
        }
        let Some(root) = bytes.strip_prefix(ROOT_MAGIC) else {
            bail!(anyhow!(FxfsError::Inconsistent).context("Invalid root magic"));
        };
        let parsed: Self = bincode::DefaultOptions::new()
            .with_limit(storage_device::MAX_ROOT as u64)
            .deserialize(root)
            .map_err(|error| {
                anyhow!(FxfsError::Inconsistent).context(format!("Bad root: {error}"))
            })?;
        parsed.validate()?;
        Ok(parsed)
    }

    pub(crate) fn validate(&self) -> Result<(), Error> {
        if let Some(sb) = &self.super_block {
            ensure!(
                !sb.is_empty()
                    && BLOCK_SIZE.is_aligned(&sb.range)
                    && sb.extent.sequence < MAX_EXTENT_SEQUENCE,
                anyhow!(FxfsError::Inconsistent)
                    .context(format!("Invalid root super_block {sb:?}"))
            );
        }
        ensure!(
            self.retired_super_block.len() <= MAX_ROOT_RETIRED,
            anyhow!(FxfsError::Inconsistent).context("Too many retired super-blocks in root")
        );
        ensure!(
            self.journal_spans.len() <= MAX_ROOT_SPANS,
            anyhow!(FxfsError::Inconsistent).context("Too many journal spans in root")
        );
        ensure!(
            BLOCK_SIZE.is_aligned(self.journal_freed_to)
                && self.journal_freed_to <= MAX_JOURNAL_OFFSET
                && self.journal_end <= MAX_JOURNAL_OFFSET,
            anyhow!(FxfsError::Inconsistent).context("Invalid journal offsets in root")
        );
        let mut prev_end = None;
        for span in &self.journal_spans {
            span.validate()?;
            if let Some(expected) = prev_end {
                ensure!(
                    span.start_offset == expected,
                    anyhow!(FxfsError::Inconsistent).context(format!(
                        "Non-contiguous journal spans in root: expected {expected}, got {}",
                        span.start_offset
                    ))
                );
            }
            prev_end = Some(span.end_offset());
        }
        if let (Some(first), Some(last)) = (self.journal_spans.first(), self.journal_spans.last()) {
            ensure!(
                first.start_offset <= self.journal_freed_to
                    && self.journal_freed_to <= last.end_offset(),
                anyhow!(FxfsError::Inconsistent).context(format!(
                    "Root journal_freed_to {} outside spans {}..{}",
                    self.journal_freed_to,
                    first.start_offset,
                    last.end_offset()
                ))
            );
        }
        Ok(())
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        use bincode::Options as _;
        let mut bytes = ROOT_MAGIC.to_vec();
        bincode::DefaultOptions::new().serialize_into(&mut bytes, self).expect("serialize root");
        bytes
    }
}

pub type SuperBlockHeader = SuperBlockHeaderV64;

#[derive(
    Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize, TypeFingerprint, Versioned,
)]
pub struct SuperBlockHeaderV64 {
    /// The globally unique identifier for the filesystem.
    pub guid: UuidWrapperV32,

    /// Monotonically increasing generation number bumped on each super-block write.
    pub generation: u64,

    /// The root parent store is an in-memory only store and serves as the backing store for the
    /// root store. The records for this store are serialized into the super-block and mutations
    /// are also recorded in the journal.
    pub root_parent_store_object_id: u64,

    /// The root parent needs a graveyard and there's nowhere else to store it other than in the
    /// super-block.
    pub root_parent_graveyard_directory_object_id: u64,

    /// The root object store contains all other metadata objects (including the allocator) and is
    /// the parent for all other object stores.
    pub root_store_object_id: u64,

    /// This is in the root object store.
    pub allocator_object_id: u64,

    /// Start checkpoint for the journal stream.
    pub journal_checkpoint: JournalCheckpointV32,

    /// Offset of the journal stream when the super-block was written. If no entry is present in
    /// journal_file_offsets for a particular object, then an object might have dependencies on the
    /// journal from super_block_journal_file_offset onwards, but not earlier.
    pub super_block_journal_file_offset: u64,

    /// object id -> journal file offset. Indicates where each object has been flushed to.
    pub journal_file_offsets: HashMap<u64, u64>,

    /// Records the amount of borrowed metadata space as applicable at
    /// `super_block_journal_file_offset`.
    pub borrowed_metadata_space: u64,

    /// The earliest version of Fxfs used to create any still-existing struct in the filesystem.
    pub earliest_version: Version,
}

type UuidWrapper = UuidWrapperV32;
#[derive(Clone, Default, Eq, PartialEq)]
pub struct UuidWrapperV32(pub Uuid);

impl UuidWrapper {
    fn new() -> Self {
        Self(Uuid::new_v4())
    }
    #[cfg(test)]
    fn nil() -> Self {
        Self(Uuid::nil())
    }
}

impl fmt::Debug for UuidWrapper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl TypeFingerprint for UuidWrapper {
    fn fingerprint() -> String {
        "<[u8;16]>".to_owned()
    }
}

impl Serialize for UuidWrapper {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.as_bytes().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for UuidWrapper {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        <[u8; 16]>::deserialize(deserializer).map(|bytes| UuidWrapperV32(Uuid::from_bytes(bytes)))
    }
}

pub type SuperBlockRecord = SuperBlockRecordV66;

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Serialize, Deserialize, TypeFingerprint, Versioned)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub enum SuperBlockRecordV66 {
    // Following the super-block header are ObjectItem records that are to be replayed into the root
    // parent object store.
    ObjectItem(ObjectItemV66),

    // Marks the end of the full super-block.
    End,
}

/// Reads the single atomic super-block and `root_parent` store from `extent` on `device`.
/// cladefs: returns `Box<ObjectStore>` so `load` stays within 16 KiB guest kernel stacks
/// (TASK-26.9).
async fn read(
    device: Arc<dyn Device>,
    block_size: BlockSize,
    extent: ExtentRange,
) -> Result<(SuperBlockHeader, Box<ObjectStore>), Error> {
    let (super_block_header, mut reader) =
        crate::boxed(|| SuperBlockHeader::read_header_from_extent(device.clone(), extent))
            .await
            .context("failed to read superblock")?;
    let root_parent = ObjectStore::new_root_parent(
        device,
        block_size,
        super_block_header.root_parent_store_object_id,
    );
    root_parent.set_graveyard_directory_object_id(
        super_block_header.root_parent_graveyard_directory_object_id,
    );

    loop {
        let mutation = match reader.next_item().await? {
            SuperBlockRecord::ObjectItem(item) => Mutation::insert_object(item.key, item.value),
            SuperBlockRecord::End => break,
        };
        // cladefs: these are checked as the journal's are.
        ensure!(
            Journal::validate_mutation(&mutation, block_size),
            anyhow!(FxfsError::Inconsistent).context("Bad record in the super-block")
        );
        root_parent.apply_mutation(
            mutation,
            &ApplyContext {
                mode: ApplyMode::Replay,
                checkpoint: JournalCheckpoint { file_offset: 0, ..Default::default() },
            },
            AssocObj::None,
        )?;
    }
    Ok((super_block_header, root_parent))
}

/// Serializes `super_block_header` and `items` into a single fresh write-once extent on `device`,
/// returning its `ExtentRange` (`0..len`). Registers the extent in `known.in_flight` before its
/// first write so concurrent leak/byte-counting oracles see it immediately; does NOT commit `Root`
/// yet.
async fn write_to_extent(
    super_block_header: &SuperBlockHeader,
    items: LayerSet<ObjectKey, ObjectValue>,
    filesystem: &FxFilesystem,
    known: Arc<Mutex<KnownSystem>>,
) -> Result<ExtentRange, Error> {
    let mut writer = JournalWriter::new(BLOCK_SIZE, 0);
    writer.write_all(SUPER_BLOCK_MAGIC)?;
    super_block_header.serialize_with_version(&mut writer)?;
    let mut merger = items.merger();
    let mut iter = LSMTree::major_iter(merger.query(Query::FullScan).await?).await?;
    while let Some(item) = iter.get() {
        SuperBlockRecord::ObjectItem(item.cloned()).serialize_into(&mut writer)?;
        iter.advance().await?;
    }
    SuperBlockRecord::End.serialize_into(&mut writer)?;
    writer.pad_to_block()?;

    let total_len = writer.flushable_bytes();
    ensure!(total_len > 0, anyhow!(FxfsError::Inconsistent).context("Empty super-block"));

    let device = filesystem.device();
    let extent = filesystem.allocator().mint_system_extent();
    known.lock().in_flight.push((extent, total_len as u64));

    // Write the entire super-block in one `device.write` so `ExtentDevice`'s write guard holds off
    // any concurrent barrier/seal until all chunks complete (preventing `SealedExtent` mid-write).
    let write_res: Result<(), Error> = async {
        let mut buf = device.allocate_buffer(total_len).await;
        writer.take_flushable(buf.as_mut());
        device.write(extent, 0, buf.as_ref()).await
    }
    .await;

    if let Err(err) = write_res {
        let _ = device.free_extents(&[extent]).await;
        known.lock().in_flight.retain(|(e, _)| *e != extent);
        return Err(err.context("writing super-block extent"));
    }

    Ok(ExtentRange::new(extent, 0..total_len as u64))
}

// Compacts and returns the *old* snapshot of the root_parent store.
// Must be performed whilst holding a writer lock.
pub fn compact_root_parent(
    root_parent_store: &ObjectStore,
) -> Result<LayerSet<ObjectKey, ObjectValue>, Error> {
    let tree = root_parent_store.tree();
    let layer_set = tree.layer_set();
    {
        let mut merger = layer_set.merger();
        let mut iter = LSMTree::major_iter(merger.query(Query::FullScan).now_or_never().unwrap()?)
            .now_or_never()
            .unwrap()?;
        let new_layer = LSMTree::new_mutable_layer();
        while let Some(item_ref) = iter.get() {
            new_layer.insert(item_ref.cloned())?;
            iter.advance().now_or_never().unwrap()?;
        }
        tree.set_mutable_layer(new_layer);
    }
    Ok(layer_set)
}

/// Synchronized state of `Root` and its unsealed reclamation watermarks.
#[derive(Default)]
struct RootState {
    /// The last `Root` durably committed via `commit_root` (or loaded at mount).
    root: Root,
    /// Sizes of retired super-block extents known to this mount (so `sync_known` can transition
    /// a superseded super-block from active to unfreed-retired atomically under one lock).
    retired_sizes: HashMap<ExtentId, u64>,
    /// Retired super-block extents for which `device.free_extents()` has already completed,
    /// waiting for the next barrier (`commit_root` / `POST_BARRIER`) to seal their frees.
    unsealed_super_blocks: Vec<ExtentId>,
    /// Block-aligned journal offset up to which superseded journal blocks have had
    /// `device.free_extents()` complete, waiting for the next barrier to seal their frees.
    unsealed_journal_freed_to: u64,
}

/// Sync snapshot of live/unfreed system extents for leak and byte-counting oracles.
#[derive(Default)]
struct KnownSystem {
    super_block: Option<ExtentRange>,
    /// New super-block extent(s) currently being written before `commit_root` publishes them.
    in_flight: Vec<(ExtentId, u64)>,
    /// Retired super-block extents not yet freed (e.g. between `commit_root` and `free_extents`,
    /// or if a free failed and will be retried).
    unfreed_retired: Vec<(ExtentId, u64)>,
    /// Journal offset up to which superseded journal blocks have already been freed on the device.
    journal_freed_to: u64,
}

/// Manages loading, atomically swapping, and reclaiming the single super-block in `Root`.
pub(super) struct SuperBlockManager {
    /// Held across every barrier that updates `Root` so roots never commit out of order.
    state: futures::lock::Mutex<RootState>,
    /// Sync mirror for `Journal::known_extents()` and `Journal::system_bytes()`.
    known: Arc<Mutex<KnownSystem>>,
}

impl SuperBlockManager {
    pub fn new() -> Self {
        Self {
            state: futures::lock::Mutex::new(RootState::default()),
            known: Arc::new(Mutex::new(KnownSystem::default())),
        }
    }

    /// Updates `Root` atomically behind a barrier. Every barrier seals frees completed before it,
    /// so any `unsealed_super_blocks` and `unsealed_journal_freed_to` are pruned from `next`
    /// before `change` runs, and cleared once `barrier` succeeds.
    pub async fn set_root(
        &self,
        change: impl FnOnce(&mut Root),
        barrier: impl AsyncFnOnce(Vec<u8>) -> Result<bool, Error>,
    ) -> Result<bool, Error> {
        let mut state = self.state.lock().await;
        let prev_sb = state.root.super_block.clone();
        let mut next = state.root.clone();
        // Every barrier seals all frees completed before it began: drop already-freed retired
        // super-blocks and advance `journal_freed_to`.
        if !state.unsealed_super_blocks.is_empty() {
            next.retired_super_block.retain(|e| !state.unsealed_super_blocks.contains(e));
        }
        if state.unsealed_journal_freed_to > next.journal_freed_to {
            next.journal_freed_to = state.unsealed_journal_freed_to;
            prune_spans_before(&mut next.journal_spans, next.journal_freed_to);
        }
        change(&mut next);
        next.validate()?;
        let encoded = next.encode();
        ensure!(
            encoded.len() <= storage_device::MAX_ROOT,
            anyhow!(FxfsError::Inconsistent)
                .context(format!("Encoded root exceeds MAX_ROOT ({} bytes)", encoded.len()))
        );
        let set = barrier(encoded).await?;
        if set {
            if let Some(old) = prev_sb {
                if next.retired_super_block.contains(&old.extent) {
                    state.retired_sizes.insert(old.extent, old.len());
                }
            }
            state.retired_sizes.retain(|e, _| next.retired_super_block.contains(e));
            state.root = next;
            state.unsealed_super_blocks.clear();
            state.unsealed_journal_freed_to =
                state.unsealed_journal_freed_to.max(state.root.journal_freed_to);
            self.sync_known(&state);
        }
        Ok(set)
    }

    fn sync_known(&self, state: &RootState) {
        let mut known = self.known.lock();
        known.super_block = state.root.super_block.clone();
        if let Some(sb_extent) = known.super_block.as_ref().map(|sb| sb.extent) {
            known.in_flight.retain(|(e, _)| *e != sb_extent);
        }
        known.unfreed_retired = state
            .root
            .retired_super_block
            .iter()
            .filter(|e| !state.unsealed_super_blocks.contains(e))
            .map(|&e| (e, state.retired_sizes.get(&e).copied().unwrap_or(BLOCK_SIZE.get())))
            .collect();
        known.journal_freed_to = state.unsealed_journal_freed_to.max(state.root.journal_freed_to);
    }

    /// Snapshot of the last committed (or loaded) `Root`.
    pub async fn root(&self) -> Root {
        self.state.lock().await.root.clone()
    }

    /// Sync snapshot of live/unfreed super-block extents and the freed journal offset watermark.
    pub fn known_system(&self) -> (Vec<(ExtentId, u64)>, u64) {
        let known = self.known.lock();
        let mut sb_extents = Vec::new();
        if let Some(sb) = &known.super_block {
            sb_extents.push((sb.extent, sb.len()));
        }
        for &(extent, len) in &known.in_flight {
            sb_extents.push((extent, len));
        }
        for &(extent, len) in &known.unfreed_retired {
            sb_extents.push((extent, len));
        }
        (sb_extents, known.journal_freed_to)
    }

    /// Physical bytes currently held (or buffered to be written) by system extents: active,
    /// in-flight, and unfreed retired super-blocks plus unfreed journal blocks up to
    /// `journal_written_end`. Leaf lock only; safe to call from `Allocator::needs_sync`.
    pub fn system_bytes(&self, journal_written_end: u64) -> u64 {
        let known = self.known.lock();
        let mut total = known.super_block.as_ref().map_or(0, ExtentRange::len);
        for &(_, len) in &known.in_flight {
            total = total.saturating_add(len);
        }
        for &(_, len) in &known.unfreed_retired {
            total = total.saturating_add(len);
        }
        total.saturating_add(journal_written_end.saturating_sub(known.journal_freed_to))
    }

    /// cladefs: the journal's blocks unfreed, up to `journal_written_end`: those of `system_bytes`
    /// that the journal holds, not a super-block.
    pub fn journal_bytes(&self, journal_written_end: u64) -> u64 {
        journal_written_end.saturating_sub(self.known.lock().journal_freed_to)
    }

    /// Resets unsealed state after mount replay has already freed & sealed all retired extents.
    pub async fn reset_after_mount_commit(
        &self,
        journal_spans: Vec<JournalSpan>,
        journal_freed_to: u64,
        journal_end: u64,
        device: &dyn Device,
    ) -> Result<(), Error> {
        let mut state = self.state.lock().await;
        let mut next = state.root.clone();
        next.retired_super_block.clear();
        next.journal_spans = journal_spans;
        next.journal_freed_to = journal_freed_to;
        next.journal_end = next.journal_end.max(journal_end);
        next.validate()?;
        let encoded = next.encode();
        ensure!(
            encoded.len() <= storage_device::MAX_ROOT,
            anyhow!(FxfsError::Inconsistent)
                .context(format!("Encoded mount root exceeds MAX_ROOT ({} bytes)", encoded.len()))
        );
        device.commit_root(&encoded).await.context("Failed to commit mount root")?;
        state.root = next;
        state.unsealed_super_blocks.clear();
        state.unsealed_journal_freed_to = journal_freed_to;
        self.sync_known(&state);
        Ok(())
    }

    /// Loads the single super-block and `root_parent` ObjectStore from `Root::super_block`.
    pub async fn load(
        &self,
        device: Arc<dyn Device>,
        block_size: BlockSize,
    ) -> Result<(SuperBlockHeader, Box<ObjectStore>), Error> {
        debug_assert!(MIN_SUPER_BLOCK_SIZE == block_size);

        let root = Root::read(&*device).await?;
        let extent = root
            .super_block
            .clone()
            .ok_or_else(|| anyhow!(FxfsError::NotFound).context("No super-block in the root"))?;
        {
            let mut state = self.state.lock().await;
            state.root = root;
            state.unsealed_super_blocks.clear();
            state.unsealed_journal_freed_to = state.root.journal_freed_to;
            self.sync_known(&state);
        }
        let (super_block, root_parent) = crate::boxed(|| read(device, block_size, extent)).await?;
        info!(super_block:?; "loaded super-block");
        Ok((super_block, root_parent))
    }

    /// Writes a new super-block extent, swaps `Root::super_block` atomically via `commit_root`,
    /// and then frees the superseded super-block extent and superseded journal block extents.
    pub async fn save(
        &self,
        super_block_header: SuperBlockHeader,
        filesystem: Arc<FxFilesystem>,
        root_parent: LayerSet<ObjectKey, ObjectValue>,
        journal_handle: &JournalExtentHandle,
        journal_end: u64,
    ) -> Result<(), Error> {
        let device = filesystem.device();
        let cell = device.cell();
        // If prior `free_extents` failures left `retired_super_block` full, retry freeing them
        // before adding another retired super-block so `commit_root` never exceeds
        // `MAX_ROOT_RETIRED`.
        let pending_retired: Vec<ExtentId> = {
            let state = self.state.lock().await;
            let remaining = state
                .root
                .retired_super_block
                .iter()
                .filter(|e| !state.unsealed_super_blocks.contains(e))
                .count();
            if remaining >= MAX_ROOT_RETIRED {
                state
                    .root
                    .retired_super_block
                    .iter()
                    .copied()
                    .filter(|e| e.cell == cell && !state.unsealed_super_blocks.contains(e))
                    .collect()
            } else {
                Vec::new()
            }
        };
        if !pending_retired.is_empty() {
            let results = device.free_extents(&pending_retired).await;
            let mut state = self.state.lock().await;
            for (&extent, res) in pending_retired.iter().zip(results) {
                if res.is_ok() && !state.unsealed_super_blocks.contains(&extent) {
                    state.unsealed_super_blocks.push(extent);
                }
            }
            self.sync_known(&state);
        }

        let header = super_block_header.clone();
        let fs_for_write = filesystem.clone();
        let known_for_write = self.known.clone();
        let new_extent = crate::offload(move || async move {
            write_to_extent(&header, root_parent, &fs_for_write, known_for_write).await
        })
        .await?;

        let current_spans = journal_handle.spans();

        // Atomically swap `Root::super_block` and sync `journal_spans`.
        let commit_res = crate::boxed(|| {
            self.set_root(
                |root| {
                    if let Some(old) = root.super_block.replace(new_extent.clone()) {
                        if old.extent.cell == cell
                            && !root.retired_super_block.contains(&old.extent)
                        {
                            root.retired_super_block.push(old.extent);
                        }
                    }
                    let mut spans = current_spans;
                    prune_spans_before(&mut spans, root.journal_freed_to);
                    root.journal_spans = spans;
                    root.journal_end = root.journal_end.max(journal_end);
                },
                async |root| device.commit_root(&root).await.map(|()| true),
            )
        })
        .await
        .context("Failed to commit the root");

        if let Err(err) = commit_res {
            let _ = device.free_extents(&[new_extent.extent]).await;
            self.known.lock().in_flight.retain(|(e, _)| *e != new_extent.extent);
            return Err(err);
        }

        // Now that `commit_root` has atomically switched to `new_extent`, free the superseded
        // super-block(s) and superseded journal blocks of this cell. Any failure leaves them in
        // `Root` (and unsealed watermarks unchanged) so a later compaction or mount frees them.
        let reclaim_to = BLOCK_SIZE.align_down(super_block_header.journal_checkpoint.file_offset);
        let (sb_to_free, journal_from, journal_to_free) = {
            let state = self.state.lock().await;
            let sb_to_free: Vec<ExtentId> = state
                .root
                .retired_super_block
                .iter()
                .copied()
                .filter(|e| e.cell == cell && !state.unsealed_super_blocks.contains(e))
                .collect();
            let from = state.unsealed_journal_freed_to.max(state.root.journal_freed_to);
            let journal_to_free: Vec<ExtentId> = if reclaim_to > from {
                journal_handle
                    .extents_in(from..reclaim_to)
                    .into_iter()
                    .filter(|e| e.cell == cell)
                    .collect()
            } else {
                Vec::new()
            };
            (sb_to_free, from, journal_to_free)
        };

        let mut all_to_free = Vec::with_capacity(sb_to_free.len() + journal_to_free.len());
        all_to_free.extend_from_slice(&sb_to_free);
        all_to_free.extend_from_slice(&journal_to_free);

        if !all_to_free.is_empty() {
            let results = device.free_extents(&all_to_free).await;
            let (sb_results, journal_results) = results.split_at(sb_to_free.len());
            let mut state = self.state.lock().await;
            for (&extent, res) in sb_to_free.iter().zip(sb_results) {
                if res.is_ok() {
                    if !state.unsealed_super_blocks.contains(&extent) {
                        state.unsealed_super_blocks.push(extent);
                    }
                } else if let Err(err) = res {
                    warn!(extent:?, error:? = err; "Failed to free retired super-block; will retry");
                }
            }
            if reclaim_to > journal_from && journal_results.iter().all(Result::is_ok) {
                state.unsealed_journal_freed_to = state.unsealed_journal_freed_to.max(reclaim_to);
                journal_handle.prune_before(state.unsealed_journal_freed_to);
            } else if let Some(Err(err)) = journal_results.iter().find(|r| r.is_err()) {
                warn!(error:? = err; "Failed to free superseded journal blocks; will retry");
            }
            self.sync_known(&state);
        } else if reclaim_to > journal_from {
            // Superseded blocks belonged to a parent cell (e.g. fork) and need no local free.
            let mut state = self.state.lock().await;
            state.unsealed_journal_freed_to = state.unsealed_journal_freed_to.max(reclaim_to);
            journal_handle.prune_before(state.unsealed_journal_freed_to);
            self.sync_known(&state);
        }
        Ok(())
    }
}

impl SuperBlockHeader {
    /// Creates a new instance with random GUID.
    pub fn new(
        generation: u64,
        root_parent_store_object_id: u64,
        root_parent_graveyard_directory_object_id: u64,
        root_store_object_id: u64,
        allocator_object_id: u64,
        journal_checkpoint: JournalCheckpoint,
        earliest_version: Version,
    ) -> Self {
        SuperBlockHeader {
            guid: UuidWrapper::new(),
            generation,
            root_parent_store_object_id,
            root_parent_graveyard_directory_object_id,
            root_store_object_id,
            allocator_object_id,
            journal_checkpoint,
            earliest_version,
            ..Default::default()
        }
    }

    /// Reads the super-block header from `Root::super_block` on `device`.
    #[cfg(any(test, fuzz))]
    pub(crate) async fn read_header(
        device: Arc<dyn Device>,
    ) -> Result<(SuperBlockHeader, RecordReader), Error> {
        let extent = Root::read(&*device)
            .await?
            .super_block
            .ok_or_else(|| anyhow!(FxfsError::NotFound).context("No super-block in the root"))?;
        Self::read_header_from_extent(device, extent).await
    }

    /// Reads the super-block header from `extent`, returning it and a reader for `root_parent`
    /// records.
    async fn read_header_from_extent(
        device: Arc<dyn Device>,
        extent: ExtentRange,
    ) -> Result<(SuperBlockHeader, RecordReader), Error> {
        let handle = ExtentObjectHandle::new(device, extent);
        let mut reader = JournalReader::new(handle, &JournalCheckpoint::default());
        reader.set_eof_ok();

        reader.fill_buf().await?;

        let mut super_block_header;
        let super_block_version;
        reader.consume({
            let mut cursor = std::io::Cursor::new(reader.buffer());
            let mut magic_bytes: [u8; 8] = [0; 8];
            cursor.read_exact(&mut magic_bytes)?;
            if magic_bytes.as_slice() != SUPER_BLOCK_MAGIC.as_slice() {
                bail!("Invalid magic: {:?}", magic_bytes);
            }
            super_block_version = Version::deserialize_from(&mut cursor)?;
            // cladefs: each refusal is `FxfsError::InvalidVersion`, as a layer's of a later version
            // is, so that a mount of a filesystem outside `EARLIEST_SUPPORTED_VERSION..=LATEST_VERSION`
            // says so (doc-28, board 10050).
            if !(EARLIEST_SUPPORTED_VERSION..=LATEST_VERSION).contains(&super_block_version) {
                bail!(
                    anyhow!(FxfsError::InvalidVersion).context(format!(
                        "Unsupported SuperBlock version: {:?}",
                        super_block_version
                    ))
                );
            }
            super_block_header =
                SuperBlockHeader::deserialize_from_version(&mut cursor, super_block_version)?;

            // Ensure all store IDs are distinct.
            let mut stores = HashSet::new();
            ensure!(
                stores.insert(super_block_header.root_parent_store_object_id),
                FxfsError::Inconsistent
            );
            ensure!(
                stores.insert(super_block_header.root_store_object_id),
                FxfsError::Inconsistent
            );

            // Ensure all objects in the root parent store are distinct.
            let mut root_parent_objects = HashSet::new();
            ensure!(
                root_parent_objects
                    .insert(super_block_header.root_parent_graveyard_directory_object_id),
                FxfsError::Inconsistent
            );
            ensure!(
                root_parent_objects.insert(super_block_header.root_store_object_id),
                FxfsError::Inconsistent
            );

            // cladefs: none is the invalid ID: a graveyard's, what adds to it asserts as much.
            ensure!(
                [
                    super_block_header.root_parent_store_object_id,
                    super_block_header.root_parent_graveyard_directory_object_id,
                    super_block_header.root_store_object_id,
                    super_block_header.allocator_object_id,
                ]
                .iter()
                .all(|id| *id != INVALID_OBJECT_ID),
                FxfsError::Inconsistent
            );

            // The allocator (in root_store) cannot match any store ID.
            ensure!(
                !stores.contains(&super_block_header.allocator_object_id),
                FxfsError::Inconsistent
            );

            // cladefs: where the journal starts is where a reader starts: within a block's contents,
            // and nowhere near the end of what offsets can count.
            let start = super_block_header.journal_checkpoint.file_offset;
            ensure!(
                start % BLOCK_SIZE < BLOCK_SIZE - std::mem::size_of::<Checksum>() as u64
                    && start <= MAX_JOURNAL_OFFSET,
                anyhow!(FxfsError::Inconsistent).context("Bad journal checkpoint")
            );

            if !(EARLIEST_SUPPORTED_VERSION..=LATEST_VERSION)
                .contains(&super_block_header.journal_checkpoint.version)
            {
                bail!(anyhow!(FxfsError::InvalidVersion).context(format!(
                    "Unsupported JournalCheckpoint version: {:?}",
                    super_block_header.journal_checkpoint.version
                )));
            }

            if !(EARLIEST_SUPPORTED_VERSION..=LATEST_VERSION)
                .contains(&super_block_header.earliest_version)
            {
                bail!(anyhow!(FxfsError::InvalidVersion).context(format!(
                    "Filesystem contains struct with unsupported version: {:?}",
                    super_block_header.earliest_version
                )));
            }

            cursor.position() as usize
        })?;

        // If guid is zeroed (e.g. in a newly imaged system), assign one randomly.
        if super_block_header.guid.0.is_nil() {
            super_block_header.guid = UuidWrapper::new();
        }
        reader.set_version(super_block_version);
        Ok((super_block_header, RecordReader { reader }))
    }
}

pub struct RecordReader {
    reader: JournalReader,
}

impl RecordReader {
    pub async fn next_item(&mut self) -> Result<SuperBlockRecord, Error> {
        match self.reader.deserialize().await? {
            ReadResult::Reset(_) => bail!("Unexpected reset"),
            ReadResult::ChecksumMismatch => bail!("Checksum mismatch"),
            ReadResult::Some(x) => Ok(x),
        }
    }
}

// cladefs: what the targets of crates/fxfs/fuzz run: a super-block written as a writer would have
// written it, checksums right, holding what no writer would.
#[cfg(fuzz)]
pub mod fuzz {
    use super::{Root, SUPER_BLOCK_MAGIC, SuperBlockHeader, SuperBlockRecord};
    use crate::object_store::journal::writer::JournalWriter;
    use crate::object_store::journal::{BLOCK_SIZE, JournalCheckpoint};
    use crate::serialized_types::{Version, Versioned, VersionedLatest};
    use crate::testing::fuzz::{image, mounts_without_panicking, run};
    use arbitrary::Arbitrary;
    use std::io::Write;
    use std::sync::Arc;
    use storage_device::{Device, DeviceHolder, ExtentId, ExtentRange};

    /// What changes in a good super-block: each field, each by a chance of its own, so that those
    /// left alone leave what's read past them to be found.
    #[derive(Arbitrary, Debug)]
    pub struct SuperBlockChanges {
        generation: Option<u64>,
        root_parent_store_object_id: Option<u64>,
        root_parent_graveyard_directory_object_id: Option<u64>,
        root_store_object_id: Option<u64>,
        allocator_object_id: Option<u64>,
        journal_checkpoint: Option<JournalCheckpoint>,
        file_offset: Option<u64>,
        super_block_journal_file_offset: Option<u64>,
        journal_file_offsets: Vec<(u64, u64)>,
        borrowed_metadata_space: Option<u64>,
        earliest_version: Option<Version>,
        /// Records each put in place of the one at an index, or before the `End`.
        records: Vec<(u8, SuperBlockRecord)>,
        /// Records lost, at an index each.
        lost: Vec<u8>,
    }

    /// Replaces `Root::super_block` with `bytes`, keeping the existing journal spans in `Root`.
    async fn replace_super_block(device: &Arc<dyn Device>, bytes: &[u8]) {
        let mut writer = JournalWriter::new(BLOCK_SIZE, 0);
        writer.write_all(bytes).expect("writing");
        writer.pad_to_block().expect("padding");
        let mut buffer = device.allocate_buffer(writer.flushable_bytes()).await;
        writer.take_flushable(buffer.as_mut());
        let extent = ExtentId::new(device.cell(), device.next_sequence().await.expect("numbers"));
        device.write(extent, 0, buffer.as_ref()).await.expect("writing");
        let range = 0..buffer.len() as u64;
        let mut root = Root::read(&**device).await.unwrap_or_default();
        root.super_block = Some(ExtentRange::new(extent, range));
        device.commit_root(&root.encode()).await.expect("committing");
    }

    /// A super-block whose bytes after its magic are `input`.
    pub fn fuzz_super_block_bytes(input: Vec<u8>) {
        run(async move {
            let device = DeviceHolder::new(image());
            let bytes = [SUPER_BLOCK_MAGIC.as_slice(), &input].concat();
            replace_super_block(&device, &bytes[..bytes.len().min(1 << 20)]).await;
            mounts_without_panicking(device).await;
        });
    }

    /// The super-block of a filesystem, as `changes` leaves it.
    pub fn fuzz_super_block(changes: SuperBlockChanges) {
        run(async move {
            let device = DeviceHolder::new(image());
            let (mut header, mut records) = read_all(&device).await;
            let set = |field: &mut u64, value: Option<u64>| *field = value.unwrap_or(*field);
            set(&mut header.generation, changes.generation);
            set(&mut header.root_parent_store_object_id, changes.root_parent_store_object_id);
            set(
                &mut header.root_parent_graveyard_directory_object_id,
                changes.root_parent_graveyard_directory_object_id,
            );
            set(&mut header.root_store_object_id, changes.root_store_object_id);
            set(&mut header.allocator_object_id, changes.allocator_object_id);
            header.journal_checkpoint =
                changes.journal_checkpoint.unwrap_or(header.journal_checkpoint);
            set(&mut header.journal_checkpoint.file_offset, changes.file_offset);
            set(
                &mut header.super_block_journal_file_offset,
                changes.super_block_journal_file_offset,
            );
            header.journal_file_offsets.extend(changes.journal_file_offsets);
            set(&mut header.borrowed_metadata_space, changes.borrowed_metadata_space);
            header.earliest_version = changes.earliest_version.unwrap_or(header.earliest_version);
            for lost in changes.lost {
                let index = lost as usize;
                if index < records.len() {
                    records.remove(index);
                }
            }
            for (index, record) in changes.records {
                match records.get_mut(index as usize) {
                    Some(replaced) => *replaced = record,
                    None => records.push(record),
                }
            }
            records.push(SuperBlockRecord::End);
            let mut bytes = SUPER_BLOCK_MAGIC.to_vec();
            header.serialize_with_version(&mut bytes).expect("serializing");
            for record in &records {
                record.serialize_into(&mut bytes).expect("serializing");
            }
            replace_super_block(&device, &bytes).await;
            mounts_without_panicking(device).await;
        });
    }

    /// The super-block of `device`, and its records but for its `End`.
    async fn read_all(device: &Arc<dyn Device>) -> (SuperBlockHeader, Vec<SuperBlockRecord>) {
        let (header, mut reader) =
            SuperBlockHeader::read_header(device.clone()).await.expect("a super-block");
        let mut records = Vec::new();
        loop {
            match reader.next_item().await.expect("a record") {
                SuperBlockRecord::End => break,
                record => records.push(record),
            }
        }
        (header, records)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        JournalSpan, MAX_SPAN_BLOCKS, MIN_SUPER_BLOCK_SIZE, Root, SUPER_BLOCK_MAGIC,
        SuperBlockHeader, SuperBlockManager, SuperBlockRecord, UuidWrapper,
    };
    use crate::errors::FxfsError;
    use crate::filesystem::{FxFilesystem, SyncOptions};
    use crate::object_store::journal::JournalCheckpoint;
    use crate::object_store::journal::writer::JournalWriter;
    use crate::object_store::object_record::ObjectItem;
    use crate::object_store::transaction::{Options, lock_keys};
    use crate::object_store::{
        AttributeId, HandleOptions, ObjectHandle, ObjectKey, ObjectStore, ObjectValue,
    };
    use crate::serialized_types::{
        EARLIEST_SUPPORTED_VERSION, LATEST_VERSION, Version, Versioned, VersionedLatest,
    };
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use storage_device::buffer::BufferRef;
    use storage_device::fake_device::{FakeDevice, Op};
    use storage_device::{Device, DeviceHolder, ExtentId, ExtentRange};
    use storage_units::BlockSize;
    use test_case::test_case;

    const TEST_DEVICE_BLOCK_SIZE: BlockSize = BlockSize::SIZE_512B;
    const TEST_DEVICE_BLOCK_COUNT: u64 = 16384;

    /// cladefs: writes `buf` to a fresh extent and commits it as `Root::super_block` while
    /// preserving any existing journal spans in `Root`.
    async fn write_raw_super_block(device: &Arc<dyn Device>, buf: BufferRef<'_>) {
        static NEXT_SEQUENCE: AtomicU64 = AtomicU64::new(1000);
        let extent = ExtentId::new(device.cell(), NEXT_SEQUENCE.fetch_add(1, Ordering::Relaxed));
        device.write(extent, 0, buf).await.expect("write failed");
        let mut root = Root::read(&**device).await.unwrap_or_default();
        root.super_block = Some(ExtentRange::new(extent, 0..buf.len() as u64));
        device.commit_root(&root.encode()).await.expect("commit root failed");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_read_written_super_block() {
        let device = DeviceHolder::new(
            FakeDevice::new(TEST_DEVICE_BLOCK_COUNT, TEST_DEVICE_BLOCK_SIZE.get() as u32).calm(),
        );
        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");
        fs.close().await.expect("Close failed");
        let device = fs.take_device().await;
        device.reopen(false);
        let fs = FxFilesystem::open(device).await.expect("open failed");

        // Confirm that the initial super-block extent is one block.
        let root_before = Root::read(&*fs.device()).await.expect("read root failed");
        assert_eq!(root_before.super_block.as_ref().unwrap().len(), MIN_SUPER_BLOCK_SIZE);
        let gen_before = fs.super_block_header().generation;

        // Create many objects in the root parent store so the super-block grows past one write
        // chunk (multi-chunk single-extent super-block).
        let mut created_object_ids = vec![];
        const NUM_ENTRIES: u64 = 16384;
        for _ in 0..NUM_ENTRIES {
            let mut transaction = fs
                .root_store()
                .new_transaction(lock_keys![], Options::default())
                .await
                .expect("new_transaction failed");
            created_object_ids.push(
                ObjectStore::create_object(
                    &fs.object_manager().root_parent_store(),
                    &mut transaction,
                    HandleOptions::default(),
                )
                .await
                .expect("create_object failed")
                .object_id(),
            );
            transaction.commit().await.expect("commit failed");
        }

        let root_grown = Root::read(&*fs.device()).await.expect("read root failed");
        assert!(root_grown.super_block.as_ref().unwrap().len() > 131_072);

        let (written_sb, _) =
            SuperBlockHeader::read_header(fs.device()).await.expect("read failed");
        assert!(!written_sb.guid.0.is_nil());
        assert!(written_sb.generation > gen_before);
        assert_eq!(written_sb.root_store_object_id, fs.root_store().store_object_id());
        assert_eq!(written_sb.allocator_object_id, fs.allocator().object_id());
        assert_eq!(written_sb.earliest_version, LATEST_VERSION);

        // Delete all the objects we just made.
        for object_id in created_object_ids {
            let mut transaction = fs
                .root_store()
                .new_transaction(lock_keys![], Options::default())
                .await
                .expect("new_transaction failed");
            fs.object_manager()
                .root_parent_store()
                .adjust_refs(&mut transaction, object_id, -1)
                .await
                .expect("adjust_refs failed");
            transaction.commit().await.expect("commit failed");
            fs.object_manager()
                .root_parent_store()
                .tombstone_object(object_id, Options::default(), None)
                .await
                .expect("tombstone failed");
        }
        // Trigger a compaction so the super-block shrinks back to one block (no journal extents
        // live in root_parent anymore!).
        fs.journal().force_compact().await.expect("force_compact failed");
        let root_shrunk = Root::read(&*fs.device()).await.expect("read root failed");
        assert_eq!(root_shrunk.super_block.as_ref().unwrap().len(), MIN_SUPER_BLOCK_SIZE);
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_generation_wrapping_on_flush() {
        const BLOCK_SIZE: BlockSize = BlockSize::SIZE_4KIB;
        let mut device =
            DeviceHolder::new(FakeDevice::new(TEST_DEVICE_BLOCK_COUNT, BLOCK_SIZE.get() as u32));
        {
            let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");
            let root_store = fs.root_store();
            let mut transaction = fs
                .root_store()
                .new_transaction(lock_keys![], Options::default())
                .await
                .expect("new_transaction failed");
            ObjectStore::create_object(&root_store, &mut transaction, HandleOptions::default())
                .await
                .expect("create_object failed");
            transaction.commit().await.expect("commit failed");
            fs.sync(SyncOptions::default()).await.expect("sync failed");
            fs.close().await.expect("close failed");
            device = fs.take_device().await;
        }
        device.reopen(false);

        let manager = SuperBlockManager::new();
        let (mut header, _) =
            manager.load((*device).clone(), BLOCK_SIZE).await.expect("load failed");

        {
            let fs = FxFilesystem::open(device).await.expect("open failed");
            header.generation = u64::MAX;
            fs.journal()
                .super_block_manager
                .save(
                    header,
                    (*fs).clone(),
                    fs.root_parent_store().tree().layer_set(),
                    fs.journal().handle_for_test(),
                    fs.journal().file_offset(),
                )
                .await
                .expect("save failed");
            fs.close().await.expect("close failed");
            device = fs.take_device().await;
            device.reopen(false);

            let fs = FxFilesystem::open(device).await.expect("open failed");
            fs.journal().force_compact().await.expect("force_compact failed");
            fs.close().await.expect("close failed");
            device = fs.take_device().await;
        }
        device.reopen(false);

        let (header, _) = manager.load((*device).clone(), BLOCK_SIZE).await.expect("load failed");
        assert!(header.generation < 10);
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_guid_assign_on_read() {
        const BLOCK_SIZE: BlockSize = BlockSize::SIZE_4KIB;
        let device = DeviceHolder::new(FakeDevice::new(TEST_DEVICE_BLOCK_COUNT, 4096));
        let mut header = SuperBlockHeader::new(
            1,
            3,
            1000,
            4,
            5,
            JournalCheckpoint { file_offset: 1234, checksum: 5678, version: LATEST_VERSION },
            LATEST_VERSION,
        );
        header.guid = UuidWrapper::nil();
        let mut writer = JournalWriter::new(BLOCK_SIZE, 0);
        writer.write_all(SUPER_BLOCK_MAGIC).unwrap();
        header.serialize_with_version(&mut writer).unwrap();
        SuperBlockRecord::End.serialize_into(&mut writer).unwrap();
        writer.pad_to_block().unwrap();
        let mut buf = device.allocate_buffer(writer.flushable_bytes()).await;
        writer.take_flushable(buf.as_mut());
        write_raw_super_block(&device, buf.as_ref()).await;

        let super_block_header =
            SuperBlockHeader::read_header((*device).clone()).await.expect("read failed");
        assert!(!super_block_header.0.guid.0.is_nil());
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_init_wipes_superblocks() {
        let device = DeviceHolder::new(FakeDevice::new(8192, TEST_DEVICE_BLOCK_SIZE.get() as u32));

        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");
        fs.journal().force_compact().await.expect("force_compact failed");
        fs.close().await.expect("Close failed");
        let device = fs.take_device().await;
        device.reopen(false);

        let header = SuperBlockHeader::read_header(device.clone()).await.expect("read failed");
        let old_guid = header.0.guid;

        // Re-initialize the filesystem. The super-block should be for the new FS.
        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");
        fs.close().await.expect("Close failed");
        let device = fs.take_device().await;
        device.reopen(false);

        let after = SuperBlockHeader::read_header(device.clone()).await.expect("read failed");
        assert_ne!(old_guid, after.0.guid);
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_root_parent_is_compacted() {
        let device = DeviceHolder::new(FakeDevice::new(8192, TEST_DEVICE_BLOCK_SIZE.get() as u32));

        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");

        let mut transaction = fs
            .root_store()
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new_transaction failed");
        let store = fs.root_parent_store();
        let handle = ObjectStore::create_object(&store, &mut transaction, HandleOptions::default())
            .await
            .expect("create_object failed");
        store.add_to_graveyard(&mut transaction, handle.object_id());
        transaction.commit().await.expect("commit failed");

        store
            .tombstone_object(handle.object_id(), Options::default(), None)
            .await
            .expect("tombstone failed");

        let root_store = fs.root_store();
        for _ in 0..6000 {
            let mut transaction = fs
                .root_store()
                .new_transaction(lock_keys![], Options::default())
                .await
                .expect("new_transaction failed");
            ObjectStore::create_object(&root_store, &mut transaction, HandleOptions::default())
                .await
                .expect("create_object failed");
            transaction.commit().await.expect("commit failed");
        }

        assert!(
            !store
                .tree()
                .exists(&ObjectKey::object(handle.object_id()))
                .await
                .expect("exists failed")
        );
    }

    /// cladefs: writes a super-block that starts the journal at `file_offset`, whose records are
    /// `records`, and says whether it loads.
    async fn loads(
        device: &Arc<dyn Device>,
        file_offset: u64,
        records: &[SuperBlockRecord],
    ) -> bool {
        let header = SuperBlockHeader::new(
            1,
            3,
            4,
            5,
            6,
            JournalCheckpoint { file_offset, checksum: 0, version: LATEST_VERSION },
            LATEST_VERSION,
        );
        let mut writer = JournalWriter::new(BlockSize::SIZE_4KIB, 0);
        writer.write_all(SUPER_BLOCK_MAGIC).unwrap();
        header.serialize_with_version(&mut writer).unwrap();
        for record in records {
            record.serialize_into(&mut writer).unwrap();
        }
        SuperBlockRecord::End.serialize_into(&mut writer).unwrap();
        writer.pad_to_block().unwrap();
        let mut buf = device.allocate_buffer(writer.flushable_bytes()).await;
        writer.take_flushable(buf.as_mut());
        write_raw_super_block(device, buf.as_ref()).await;
        SuperBlockManager::new().load(device.clone(), BlockSize::SIZE_4KIB).await.is_ok()
    }

    /// cladefs: where a super-block starts the journal may be anything: a reader may start within a
    /// block's contents, and not near the end of what offsets can count.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_journal_checkpoint_validation() {
        let device = DeviceHolder::new(FakeDevice::new(TEST_DEVICE_BLOCK_COUNT, 4096));
        let device = (*device).clone();
        assert!(loads(&device, 4087, &[]).await);
        assert!(!loads(&device, 4088, &[]).await);
        assert!(!loads(&device, 4095, &[]).await);
        assert!(!loads(&device, u64::MAX - 4096, &[]).await);
    }

    /// cladefs: a mount of a filesystem whose super-block starts its journal anywhere is refused,
    /// wherever that is: in a block's seal, before or past the journal's spans, near the end of
    /// what offsets can count, or within a block that doesn't verify (TASK-149.23).
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_mount_with_a_journal_that_starts_anywhere() {
        let device = DeviceHolder::new(FakeDevice::new(TEST_DEVICE_BLOCK_COUNT, 4096));
        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");
        for _ in 0..64 {
            let mut transaction = fs
                .root_store()
                .new_transaction(lock_keys![], Options::default())
                .await
                .expect("new_transaction failed");
            ObjectStore::create_object(
                &fs.root_store(),
                &mut transaction,
                HandleOptions::default(),
            )
            .await
            .expect("create_object failed");
            transaction.commit().await.expect("commit failed");
        }
        fs.journal().force_compact().await.expect("compacting failed");
        fs.close().await.expect("close failed");
        let device = fs.take_device().await;
        device.reopen(false);

        let (header, mut reader) =
            SuperBlockHeader::read_header((*device).clone()).await.expect("read failed");
        let mut records = Vec::new();
        loop {
            match reader.next_item().await.expect("a record") {
                SuperBlockRecord::End => break,
                record => records.push(record),
            }
        }
        let mount_from = async |checkpoint: JournalCheckpoint| {
            let device = device.snapshot().expect("snapshot failed");
            let mut bytes = SUPER_BLOCK_MAGIC.to_vec();
            let mut header = header.clone();
            header.journal_checkpoint = checkpoint;
            header.serialize_with_version(&mut bytes).unwrap();
            for record in &records {
                record.serialize_into(&mut bytes).unwrap();
            }
            SuperBlockRecord::End.serialize_into(&mut bytes).unwrap();
            let mut writer = JournalWriter::new(BlockSize::SIZE_4KIB, 0);
            writer.write_all(&bytes).unwrap();
            writer.pad_to_block().unwrap();
            {
                let raw = (*device).clone();
                let mut buf = raw.allocate_buffer(writer.flushable_bytes()).await;
                writer.take_flushable(buf.as_mut());
                write_raw_super_block(&raw, buf.as_ref()).await;
            }
            FxFilesystem::open(device).await.map(drop)
        };
        let at =
            |file_offset| JournalCheckpoint { file_offset, ..header.journal_checkpoint.clone() };
        let start = header.journal_checkpoint.file_offset;
        for file_offset in
            [start | 4095, (start & !4095) + 4089, 1 << 40, 1 << 61, u64::MAX - 4096, u64::MAX]
        {
            assert!(mount_from(at(file_offset)).await.is_err(), "mounted at {file_offset}");
        }

        let journal_end = Root::read(&**device).await.expect("read root failed").journal_end;
        let written = start.max(header.super_block_journal_file_offset).max(journal_end);
        let past = (written / 4096 + 1) * 4096 + 8;
        let error = mount_from(at(past)).await.expect_err("mounted in a block not read");
        assert!(FxfsError::Inconsistent.matches(&error), "{error:?}");
    }

    /// cladefs: a super-block's records are checked as the journal's are: an extent's key must be on
    /// the filesystem's blocks' bounds, whatever its value.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_records_are_validated() {
        let device = DeviceHolder::new(FakeDevice::new(TEST_DEVICE_BLOCK_COUNT, 4096));
        let device = (*device).clone();
        let key = |range| ObjectKey::extent(5, AttributeId::DATA, range);
        let record = |range| {
            let (key, value) = (key(range), ObjectValue::deleted_extent());
            SuperBlockRecord::ObjectItem(ObjectItem { key, value })
        };
        assert!(loads(&device, 0, &[record(0..4096)]).await);
        assert!(!loads(&device, 0, &[record(0..512)]).await);
    }

    /// cladefs: a root's extents and journal spans are strictly validated on read, and old
    /// `FxfsRoot` magic is refused outright.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_root_validation() {
        let device = FakeDevice::new(8, 4096);
        let extent = ExtentRange::new(ExtentId::new(1, 1), 0..4096);
        let span = JournalSpan { start_offset: 0, block_count: 4, cell: 1, base_sequence: 10 };
        let root = Root {
            super_block: Some(extent.clone()),
            journal_spans: vec![span.clone()],
            ..Root::default()
        };
        device.commit_root(&root.encode()).await.expect("commit failed");
        assert_eq!(Root::read(&device).await.expect("read failed"), root);

        // Old V63 magic is refused outright.
        device.commit_root(b"FxfsRoot\x00\x00\x00").await.expect("commit failed");
        Root::read(&device).await.expect_err("old magic accepted");

        // Backwards extent range is refused.
        let backwards = ExtentRange::new(ExtentId::new(1, 1), 4096..0);
        let bad = Root { super_block: Some(backwards), ..Root::default() };
        device.commit_root(&bad.encode()).await.expect("commit failed");
        Root::read(&device).await.expect_err("backwards extent accepted");

        // Non-contiguous journal spans are refused.
        let gap =
            JournalSpan { start_offset: 8 * 4096, block_count: 4, cell: 1, base_sequence: 20 };
        let bad_spans = Root {
            super_block: Some(extent.clone()),
            journal_spans: vec![span.clone(), gap],
            ..Root::default()
        };
        device.commit_root(&bad_spans.encode()).await.expect("commit failed");
        Root::read(&device).await.expect_err("non-contiguous spans accepted");

        // Oversized span (DoS / OOM in `extents_in_spans`) is refused at the trust boundary.
        let huge = JournalSpan {
            start_offset: 0,
            block_count: MAX_SPAN_BLOCKS + 1,
            cell: 1,
            base_sequence: 10,
        };
        let bad_huge = Root {
            super_block: Some(extent.clone()),
            journal_spans: vec![huge],
            ..Root::default()
        };
        device.commit_root(&bad_huge.encode()).await.expect("commit failed");
        Root::read(&device).await.expect_err("oversized span accepted");

        // Span or super-block near `u64::MAX` (sequence exhaustion) is refused at the trust boundary.
        let exhaust =
            JournalSpan { start_offset: 0, block_count: 1, cell: 1, base_sequence: u64::MAX - 1 };
        let bad_seq =
            Root { super_block: Some(extent), journal_spans: vec![exhaust], ..Root::default() };
        device.commit_root(&bad_seq.encode()).await.expect("commit failed");
        Root::read(&device).await.expect_err("sequence-exhausting span accepted");

        let bad_sb_seq = Root {
            super_block: Some(ExtentRange::new(ExtentId::new(1, u64::MAX - 1), 0..4096)),
            journal_spans: vec![span],
            ..Root::default()
        };
        device.commit_root(&bad_sb_seq.encode()).await.expect("commit failed");
        Root::read(&device).await.expect_err("sequence-exhausting super-block accepted");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_invalid_object_ids_validation() {
        const BLOCK_SIZE: BlockSize = BlockSize::new(MIN_SUPER_BLOCK_SIZE as u32).unwrap();
        let device =
            DeviceHolder::new(FakeDevice::new(TEST_DEVICE_BLOCK_COUNT, BLOCK_SIZE.get() as u32));
        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");
        fs.close().await.expect("close");
        let device = fs.take_device().await;
        device.reopen(false);

        let device_arc = (*device).clone();
        let write_sb = |root_parent_store_object_id: u64,
                        root_parent_graveyard_directory_object_id: u64,
                        root_store_object_id: u64,
                        allocator_object_id: u64| {
            let device = device_arc.clone();
            async move {
                let mut super_block_header = SuperBlockHeader::new(
                    1,
                    root_parent_store_object_id,
                    root_parent_graveyard_directory_object_id,
                    root_store_object_id,
                    allocator_object_id,
                    JournalCheckpoint::default(),
                    LATEST_VERSION,
                );
                super_block_header.journal_checkpoint.version = LATEST_VERSION;

                let mut writer = JournalWriter::new(BLOCK_SIZE, 0);
                writer.write_all(SUPER_BLOCK_MAGIC).unwrap();
                super_block_header.serialize_with_version(&mut writer).unwrap();
                SuperBlockRecord::End.serialize_into(&mut writer).unwrap();
                writer.pad_to_block().unwrap();

                let mut buf = device.allocate_buffer(writer.flushable_bytes()).await;
                writer.take_flushable(buf.as_mut());
                write_raw_super_block(&device, buf.as_ref()).await;
            }
        };

        let manager = SuperBlockManager::new();

        // Duplicate store IDs (3, 3)
        write_sb(3, 4, 3, 5).await;
        assert!(manager.load((*device).clone(), BLOCK_SIZE).await.is_err());

        // Allocator matches root_parent_store_object_id (3, 3)
        write_sb(3, 4, 5, 3).await;
        assert!(manager.load((*device).clone(), BLOCK_SIZE).await.is_err());

        // Allocator matches root_store_object_id (5, 5)
        write_sb(3, 4, 5, 5).await;
        assert!(manager.load((*device).clone(), BLOCK_SIZE).await.is_err());

        // Zero object IDs
        for ids in [[0, 4, 5, 6], [3, 0, 5, 6], [3, 4, 0, 6], [3, 4, 5, 0]] {
            write_sb(ids[0], ids[1], ids[2], ids[3]).await;
            assert!(manager.load((*device).clone(), BLOCK_SIZE).await.is_err(), "{ids:?}");
        }

        // Valid configuration
        write_sb(3, 4, 5, 6).await;
        assert!(manager.load((*device).clone(), BLOCK_SIZE).await.is_ok());
    }

    #[derive(Clone, Copy, Debug)]
    enum Says {
        SuperBlock,
        Journal,
        Layers,
    }

    #[test_case(Says::SuperBlock; "of the super-block")]
    #[test_case(Says::Journal; "of the journal")]
    #[test_case(Says::Layers; "of the layers")]
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_mount_of_the_previous_version_is_refused(says: Says) {
        let unsupported = [
            Version { major: EARLIEST_SUPPORTED_VERSION.major - 1, minor: 0 },
            Version { major: LATEST_VERSION.major + 1, minor: 0 },
            Version { major: LATEST_VERSION.major, minor: LATEST_VERSION.minor + 1 },
        ];
        for bad_version in unsupported {
            let armed = Arc::new(AtomicBool::new(false));
            let written = Arc::new(AtomicU64::new(0));
            let mut device = FakeDevice::new(TEST_DEVICE_BLOCK_COUNT, 4096);
            let (counting, counted) = (armed.clone(), written.clone());
            device.set_op_callback(move |op| {
                if counting.load(Ordering::Relaxed) && matches!(op, Op::Write | Op::Free) {
                    counted.fetch_add(1, Ordering::Relaxed);
                }
                Ok(())
            });
            let fs =
                FxFilesystem::new_empty(DeviceHolder::new(device)).await.expect("new_empty failed");
            fs.close().await.expect("close failed");
            let device = fs.take_device().await;
            device.reopen(false);

            let (mut header, mut reader) =
                SuperBlockHeader::read_header((*device).clone()).await.expect("read failed");
            let mut records = Vec::new();
            loop {
                match reader.next_item().await.expect("a record") {
                    SuperBlockRecord::End => break,
                    record => records.push(record),
                }
            }
            let mut bytes = SUPER_BLOCK_MAGIC.to_vec();
            match says {
                Says::SuperBlock => {
                    bad_version.serialize_into(&mut bytes).unwrap();
                    header.serialize_into(&mut bytes).unwrap();
                }
                Says::Journal => {
                    header.journal_checkpoint.version = bad_version;
                    header.serialize_with_version(&mut bytes).unwrap();
                }
                Says::Layers => {
                    header.earliest_version = bad_version;
                    header.serialize_with_version(&mut bytes).unwrap();
                }
            }
            for record in &records {
                record.serialize_into(&mut bytes).unwrap();
            }
            SuperBlockRecord::End.serialize_into(&mut bytes).unwrap();
            let mut writer = JournalWriter::new(BlockSize::SIZE_4KIB, 0);
            writer.write_all(&bytes).unwrap();
            writer.pad_to_block().unwrap();
            {
                let mut buf = device.allocate_buffer(writer.flushable_bytes()).await;
                writer.take_flushable(buf.as_mut());
                write_raw_super_block(&device, buf.as_ref()).await;
            }

            let raw = (*device).clone();
            let root = raw.root().await.expect("root failed");
            armed.store(true, Ordering::Relaxed);
            let error = FxFilesystem::open(device).await.map(drop).expect_err("mounted");
            assert!(FxfsError::InvalidVersion.matches(&error), "{bad_version:?}: {error:?}");
            let named = match says {
                Says::SuperBlock => "Unsupported SuperBlock version",
                Says::Journal => "Unsupported JournalCheckpoint version",
                Says::Layers => "Filesystem contains struct with unsupported version",
            };
            let named = format!("{named}: {bad_version:?}");
            assert!(format!("{error:#}").contains(&named), "{error:#}");
            assert_eq!(written.load(Ordering::Relaxed), 0);
            raw.reopen(false);
            assert_eq!(raw.root().await.expect("root failed"), root);
        }
    }
}
