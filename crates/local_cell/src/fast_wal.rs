// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: Fast WAL commit mode backed by appendable zonal object storage (Amazon S3 Express One
//! Zone implemented via `PutObject` + `x-amz-write-offset-bytes`; GCS Rapid Storage via gRPC
//! `BidiWriteObject` is planned; doc-29; formally verified by `specs/fast-wal/FastWal.tla`).
//!
//! Compute nodes stay stateless cache-only on the commit path: commits are staged in memory with
//! zero local disk `fsync`s on the critical path, batched adaptively by [`AdaptiveBatcher`], and
//! appended via offset-checked appends (`Bucket::append`) to
//! `tenants/<T>/cells/<C>/wal/<epoch>/<segment>`. Large extents (`>= DEFAULT_LARGE_EXTENT`,
//! 256 KiB) are uploaded directly to the cold regional bucket before referencing them
//! metadata-only in the fast WAL (`largeBeforeWal`), eliminating double upload when the background
//! cold uploader closes and uploads segments.
//!
//! Safety invariants proved by `specs/fast-wal/FastWal.tla`:
//! - `HEADER_LEN = 80`: every writer WAL object starts with an 80-byte authenticated header, so
//!   writer appends only occur at `offset == 0` or `offset >= 80`.
//! - Authenticated tombstones (`CFTF`, `CFTR`, `CFTS`) are 36 bytes (`0 < 36 < 80`), bound under
//!   the cell's DEK to `(cell, epoch, segment, value)`, so overwriting superseded or reclaimed WAL
//!   objects permanently rejects delayed zombie appends while preventing forged or premature
//!   tombstones from skipping durable commits.
//! - Taker fences (`CFWF`) derive a domain-separated AEAD key per taker epoch and use the exact
//!   byte offset as the nonce, eliminating any AES-GCM nonce reuse with a concurrent old writer.
//! - Segment rollover appends `WalRecord::Rolled` strictly below `MaxParts` (`rollBeforeLimit`)
//!   before creating `seg + 1` (`rollBeforeNext`).
//! - Takeover preserves `head.fast_wal.active` (`keepOldWalInHead`), validates the active bucket
//!   URL against `CLADEFSD_FAST_WAL_ALLOWED_BUCKETS`, fences and recovers the old epoch's WAL
//!   segments across rollovers (`fenceFollowsRoll`), flushes recovered commits to cold segments,
//!   and atomically switches `head.fast_wal` in the same head CAS (`activateAfterFlush`).

use crate::replicate::{Holding, HotBudget};
use crate::upload::{Clock, Failure};
use bucket_cell::any_bucket::AnyBucket;
use bucket_cell::batch::{Commit, SegmentBuilder};
use bucket_cell::bucket::{self, Appended, Bucket};
use bucket_cell::fetch::Priority;
use bucket_cell::keys::Ring;
use bucket_cell::layout::{self, ActiveWal, CellId, FastWal, Kind, NewVersion, epoch_of};
use bucket_cell::{Bytes, Writer, rules};
use event_listener::Event;
use extent_keys::{Aead, Salt};
use fprint::TypeFingerprint;
use futures::StreamExt as _;
use futures::future::{BoxFuture, Either, FutureExt as _};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use storage_device::ExtentId;

/// Frozen fast WAL segment and tombstone format family (doc-28, doc-29, board 12297):
/// 1: 80-byte segment header (`CFWAL001`), authenticated `CFWR` writer records (`Batch`, `Rolled`),
/// domain-separated `CFWF` taker fence records, and 36-byte authenticated tombstones (`CFTF`
/// empty-segment fence, `CFTR` rolled-reclaimed with `max_commit_epoch`, `CFTS` superseded-sweep).
pub const FORMAT: formats::Format = formats::Format::new("fast wal", 1, 1);

/// Magic bytes starting every fast WAL segment header.
pub const HEADER_MAGIC: [u8; 8] = *b"CFWAL001";

/// Exact byte length of a fast WAL segment header:
/// `magic(8) + version(4) + key_gen(4) + tenant(8) + cell_id(8) + epoch(8) + seg(8) + salt(32)`.
/// Writer appends only occur at `offset == 0` or `offset >= HEADER_LEN`.
pub const HEADER_LEN: usize = 80;

/// Exact byte length of an authenticated fast WAL tombstone (`0 < 36 < HEADER_LEN`, permanently
/// rejecting delayed zombie appends at offset 0 or `offset >= HEADER_LEN`, board 12299):
/// `magic(4) + version(4) + key_gen(4) + value(8) + tag(16)`.
pub const TOMBSTONE_LEN: usize = 36;

/// Magic bytes for an authenticated empty-segment fence tombstone (`append(offset=0)` only).
const EMPTY_FENCE_MAGIC: [u8; 4] = *b"CFTF";

/// Magic bytes for an authenticated rolled-and-reclaimed tombstone (`overwrite` after cold head ack;
/// carries `max_commit_epoch` verified `<= last_seen_epoch` during takeover recovery).
const ROLLED_TOMBSTONE_MAGIC: [u8; 4] = *b"CFTR";

/// Magic bytes for an authenticated superseded-epoch sweep tombstone (`overwrite` only after
/// `head.fast_wal.active` has moved past this epoch; refused if ever seen on an active epoch).
const SWEPT_TOMBSTONE_MAGIC: [u8; 4] = *b"CFTS";

/// Authenticated 36-byte tombstone kinds stored at a fast WAL segment key (board 12299).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tombstone {
    /// Segment did not exist at takeover; fenced at offset 0 by `by_epoch > wal_epoch`.
    EmptyFence { by_epoch: u64 },
    /// Rolled segment reclaimed after cold head acknowledged through `max_commit_epoch`.
    Rolled { max_commit_epoch: u64 },
    /// Superseded epoch swept after head moved to `head_epoch > wal_epoch`.
    Swept { head_epoch: u64 },
}

/// Maximum fast WAL segment object size accepted before downloading during recovery (bounds
/// takeover memory before `GET`, Hotbed review 118).
const MAX_RECOVERY_WAL_OBJECT_BYTES: u64 = (MAX_RECORD_PAYLOAD as u64) + (HEADER_LEN as u64) + 4096;

/// Maximum cold own-extent object size accepted before downloading during recovery.
const MAX_RECOVERY_OWN_EXTENT_BYTES: u64 = (MAX_RECORD_PAYLOAD as u64) + 65_536;

/// Default large-write threshold (256 KiB): extents at or above this size are uploaded directly to
/// the cold regional bucket before referencing them metadata-only in the fast WAL.
pub const DEFAULT_LARGE_EXTENT: u64 = 256 << 10;

/// Default maximum writer appends per fast WAL segment before proactively rolling (`rollBeforeLimit`,
/// leaving headroom below S3 Express One Zone's 10,000-part limit for `Rolled` or taker `Fence`).
pub const DEFAULT_MAX_WRITER_APPENDS: u32 = 5_000;

/// Default maximum fast WAL segment size before proactively rolling (32 MiB).
pub const DEFAULT_MAX_SEGMENT_BYTES: u64 = 32 << 20;

/// Maximum serialized payload bytes accumulated in one adaptive group-commit batch.
pub const MAX_BATCH_BYTES: usize = 512 << 10;

/// Maximum commits accumulated in one adaptive group-commit batch.
pub const MAX_BATCH_COMMITS: usize = 64;

/// Minimum adaptive linger window under concurrent load.
pub const MIN_LINGER: Duration = Duration::from_micros(250);

/// Maximum adaptive linger window under concurrent load.
pub const MAX_LINGER: Duration = Duration::from_millis(2);

/// Maximum size of a single decoded fast WAL record payload (256 MiB).
const MAX_RECORD_PAYLOAD: usize = 256 << 20;

/// Maximum size of a cold recovery segment before cutting a new segment during takeover flush.
const RECOVERY_SEGMENT_BYTES: usize = 32 << 20;

/// Writer record prefix magic bytes (`Batch`, `Rolled`).
const RECORD_MAGIC: [u8; 4] = *b"CFWR";

/// Taker fence record prefix magic bytes (domain-separated AEAD key + offset nonce, zero nonce
/// reuse with concurrent writer appends).
const FENCE_MAGIC: [u8; 4] = *b"CFWF";

/// How many times in a row a take's fence may be refused at a segment's end, nobody appending,
/// before the take gives up on it as at its appends' limit ([`fence_and_recover`]): an object
/// store's backoff between them (50 ms doubling to 5 s on `ObjectBucket`) outlasts any write in
/// progress that S3 refuses a conflicting one for (409).
const FENCE_REFUSALS: u32 = 8;

/// Concurrent cold-bucket PUTs for large extents within one batch.
const LARGE_PUTS_AT_ONCE: usize = 16;

/// Opens a fast WAL bucket by its URL (for both the daemon's local zonal bucket and cross-AZ
/// takeover recovery of another zone's bucket).
pub trait FastWalOpener: Send + Sync + 'static {
    /// Checks whether `url` is permitted by this host's allowlist before taking a cell's cold head
    /// (doc-29). Implementations with an explicit allowlist override this so untrusted URLs fail
    /// hard without advancing the cell's placement token.
    fn is_allowed(&self, _url: &str) -> Result<(), String> {
        Ok(())
    }

    fn open(&self, url: &str) -> BoxFuture<'static, Result<Arc<AnyBucket>, String>>;
}

/// Configuration passed into `upload::Options` for fast WAL cells.
#[derive(Clone)]
pub struct FastWalConfig {
    /// URL of this daemon's local appendable zonal bucket (`CLADEFSD_FAST_WAL_BUCKET`).
    pub bucket_url: String,
    /// Already-opened and append-probed local fast WAL bucket: an object store's in production,
    /// the simulation's in its world (board 14433).
    pub bucket: Arc<AnyBucket>,
    /// Opener for cross-AZ fast WAL buckets named in a previous epoch's `head.fast_wal.active`.
    pub opener: Arc<dyn FastWalOpener>,
    /// Maximum writer appends per WAL segment before rolling (defaults to
    /// [`DEFAULT_MAX_WRITER_APPENDS`]).
    pub max_writer_appends: u32,
    /// Maximum bytes per WAL segment before rolling (defaults to [`DEFAULT_MAX_SEGMENT_BYTES`]).
    pub max_segment_bytes: u64,
}

impl std::fmt::Debug for FastWalConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FastWalConfig")
            .field("bucket_url", &self.bucket_url)
            .field("max_writer_appends", &self.max_writer_appends)
            .field("max_segment_bytes", &self.max_segment_bytes)
            .finish_non_exhaustive()
    }
}

impl FastWalConfig {
    /// Creates a config with default rollover limits and the standard URL opener.
    pub fn new(bucket_url: String, bucket: Arc<AnyBucket>, opener: Arc<dyn FastWalOpener>) -> Self {
        Self {
            bucket_url,
            bucket,
            opener,
            max_writer_appends: DEFAULT_MAX_WRITER_APPENDS,
            max_segment_bytes: DEFAULT_MAX_SEGMENT_BYTES,
        }
    }
}

/// Live fast WAL status reported via `UploadStatus` and `StatCell`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FastWalStatus {
    pub bucket: String,
    pub degraded: bool,
    pub durable: u64,
}

/// A large own-extent staged by `LocalCell::flush_locked` for direct cold-bucket upload before
/// writing a metadata-only [`LargeRef`] to the fast WAL.
#[derive(Clone, Debug)]
pub struct StagedLargeExtent {
    pub sequence: u64,
    pub logical_len: u64,
    pub bucket_segment: u64,
    pub framed_bytes: Vec<u8>,
}

/// A single committed transaction staged by `LocalCell::flush_locked` for the fast WAL writer.
#[derive(Clone, Debug)]
pub struct StagedCommit {
    pub epoch: u64,
    pub tag: u64,
    pub inline_extents: Vec<(ExtentId, Vec<u8>)>,
    pub large_extents: Vec<StagedLargeExtent>,
    pub frees: Vec<ExtentId>,
    pub root: Option<Vec<u8>>,
    pub capacity: Option<u64>,
    pub next_sequence: u64,
    pub versions: Vec<NewVersion>,
}

impl StagedCommit {
    fn estimated_wal_bytes(&self) -> usize {
        let inline: usize = self.inline_extents.iter().map(|(_, b)| b.len() + 24).sum();
        let large = self.large_extents.len() * 32;
        let frees = self.frees.len() * 16;
        let root = self.root.as_ref().map_or(0, Vec::len);
        inline + large + frees + root + 64
    }

    /// Total heap bytes held by this staged commit (inline extents + pre-framed large extents),
    /// charged against the host's upload [`Budget`] until flushed (Hotbed review 118).
    fn staged_heap_bytes(&self) -> u64 {
        let large: usize = self.large_extents.iter().map(|l| l.framed_bytes.len()).sum();
        self.estimated_wal_bytes().saturating_add(large) as u64
    }

    fn into_wire_commit(self) -> FastWalCommit {
        FastWalCommit {
            epoch: self.epoch,
            tag: self.tag,
            inline_extents: self.inline_extents,
            large_refs: self
                .large_extents
                .into_iter()
                .map(|l| LargeRef {
                    sequence: l.sequence,
                    logical_len: l.logical_len,
                    bucket_segment: l.bucket_segment,
                })
                .collect(),
            frees: self.frees,
            root: self.root,
            capacity: self.capacity,
            next_sequence: self.next_sequence,
            versions: self.versions,
        }
    }
}

/// Metadata-only reference to a large own-extent already uploaded to the cold regional bucket at
/// `layout::extent(cell, wal_epoch, bucket_segment, ExtentId::new(cell.id, sequence))`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub struct LargeRef {
    pub sequence: u64,
    pub logical_len: u64,
    pub bucket_segment: u64,
}

/// One commit recorded inside a [`WalRecord::Batch`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub struct FastWalCommit {
    pub epoch: u64,
    pub tag: u64,
    pub inline_extents: Vec<(ExtentId, Vec<u8>)>,
    pub large_refs: Vec<LargeRef>,
    pub frees: Vec<ExtentId>,
    pub root: Option<Vec<u8>>,
    pub capacity: Option<u64>,
    pub next_sequence: u64,
    pub versions: Vec<NewVersion>,
}

/// Record stored inside an encrypted, authenticated fast WAL segment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub enum WalRecord {
    Batch(Vec<FastWalCommit>),
    Rolled,
    Fence { by_epoch: u64 },
}

/// Decoded 80-byte fast WAL segment header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WalHeader {
    pub key_generation: u32,
    pub cell: CellId,
    pub epoch: u64,
    pub segment: u64,
    pub salt: Salt,
}

/// Untrusted wire parsing and serialization for fast WAL objects.
/// Enforces `#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]` per AGENTS.md.
mod wire {
    #![deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]

    use super::{
        EMPTY_FENCE_MAGIC, FENCE_MAGIC, FORMAT, HEADER_LEN, HEADER_MAGIC, MAX_RECORD_PAYLOAD,
        RECORD_MAGIC, ROLLED_TOMBSTONE_MAGIC, SWEPT_TOMBSTONE_MAGIC, TOMBSTONE_LEN, Tombstone,
        WalHeader, WalRecord,
    };
    use crate::untrusted::Cursor;
    use bincode::Options as _;
    use bucket_cell::Error;
    use bucket_cell::keys::Ring;
    use bucket_cell::layout::CellId;
    use extent_frame::Keys as _;
    use extent_keys::{Aead, KeyId, Salt, TAG_LEN};

    pub(super) fn encode_header(header: &WalHeader) -> [u8; HEADER_LEN] {
        let mut out = Vec::with_capacity(HEADER_LEN);
        out.extend_from_slice(&HEADER_MAGIC);
        out.extend_from_slice(&FORMAT.current.to_le_bytes());
        out.extend_from_slice(&header.key_generation.to_le_bytes());
        out.extend_from_slice(&header.cell.tenant.to_le_bytes());
        out.extend_from_slice(&header.cell.id.to_le_bytes());
        out.extend_from_slice(&header.epoch.to_le_bytes());
        out.extend_from_slice(&header.segment.to_le_bytes());
        out.extend_from_slice(&header.salt.0);
        out.try_into().expect("fast WAL header is exactly HEADER_LEN bytes")
    }

    fn tombstone_parts(tombstone: Tombstone) -> ([u8; 4], u64) {
        match tombstone {
            Tombstone::EmptyFence { by_epoch } => (EMPTY_FENCE_MAGIC, by_epoch),
            Tombstone::Rolled { max_commit_epoch } => (ROLLED_TOMBSTONE_MAGIC, max_commit_epoch),
            Tombstone::Swept { head_epoch } => (SWEPT_TOMBSTONE_MAGIC, head_epoch),
        }
    }

    fn derive_tombstone_aead(
        cell: CellId,
        epoch: u64,
        segment: u64,
        keys: &Ring,
        key_generation: u32,
        magic: [u8; 4],
        value: u64,
    ) -> Result<Aead, Error> {
        let key_id = KeyId { cell: cell.id, generation: key_generation };
        let cell_keys = keys.of(cell.id);
        let dek = cell_keys.dek(key_id).ok_or(Error::NoKey(key_id))?;
        let mut salt = [0u8; 32];
        salt[..8].copy_from_slice(&cell.tenant.to_le_bytes());
        salt[8..16].copy_from_slice(&cell.id.to_le_bytes());
        salt[16..24].copy_from_slice(&epoch.to_le_bytes());
        salt[24..32].copy_from_slice(&segment.to_le_bytes());
        let subkey = dek.derive(
            &Salt(salt),
            &[b"cladefs-fast-wal-tombstone".as_slice(), &magic, &value.to_le_bytes()],
        );
        Ok(Aead::new(&subkey))
    }

    fn tombstone_aad(
        magic: [u8; 4],
        version: u32,
        key_generation: u32,
        cell: CellId,
        epoch: u64,
        segment: u64,
        value: u64,
    ) -> [u8; 52] {
        let mut aad = Vec::with_capacity(52);
        aad.extend_from_slice(&magic);
        aad.extend_from_slice(&version.to_le_bytes());
        aad.extend_from_slice(&key_generation.to_le_bytes());
        aad.extend_from_slice(&cell.tenant.to_le_bytes());
        aad.extend_from_slice(&cell.id.to_le_bytes());
        aad.extend_from_slice(&epoch.to_le_bytes());
        aad.extend_from_slice(&segment.to_le_bytes());
        aad.extend_from_slice(&value.to_le_bytes());
        aad.try_into().expect("tombstone AAD is 52 bytes")
    }

    /// Encodes a 36-byte authenticated tombstone bound to `(cell, epoch, segment, tombstone)`
    /// under the cell's encrypting DEK (board 12299).
    pub(super) fn encode_tombstone(
        cell: CellId,
        epoch: u64,
        segment: u64,
        keys: &Ring,
        tombstone: Tombstone,
    ) -> Result<[u8; TOMBSTONE_LEN], Error> {
        let (_, key_id) = keys.try_encrypting()?;
        let (magic, value) = tombstone_parts(tombstone);
        let version = FORMAT.current;
        let aead =
            derive_tombstone_aead(cell, epoch, segment, keys, key_id.generation, magic, value)?;
        let aad = tombstone_aad(magic, version, key_id.generation, cell, epoch, segment, value);
        let mut empty = [];
        let tag = aead.seal(0, &aad, &mut empty);
        let mut out = Vec::with_capacity(TOMBSTONE_LEN);
        out.extend_from_slice(&magic);
        out.extend_from_slice(&version.to_le_bytes());
        out.extend_from_slice(&key_id.generation.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
        out.extend_from_slice(&tag);
        Ok(out.try_into().expect("fast WAL tombstone is exactly TOMBSTONE_LEN bytes"))
    }

    /// Decodes and cryptographically authenticates a 36-byte tombstone for `(cell, epoch, segment)`
    /// (board 12299). Refuses any forged, corrupted, replayed, or unsupported-version tombstone.
    pub(super) fn decode_tombstone(
        bytes: &[u8],
        expected_cell: CellId,
        expected_epoch: u64,
        expected_segment: u64,
        keys: &Ring,
    ) -> Result<Tombstone, Error> {
        if bytes.len() != TOMBSTONE_LEN {
            return Err(Error::Corrupt(format!(
                "fast WAL tombstone length {}, expected {TOMBSTONE_LEN}",
                bytes.len()
            )));
        }
        let mut cursor = Cursor::new(bytes);
        let magic =
            cursor.array::<4>().ok_or_else(|| Error::Corrupt("fast WAL tombstone magic".into()))?;
        let version =
            cursor.u32().ok_or_else(|| Error::Corrupt("fast WAL tombstone version".into()))?;
        FORMAT
            .check(version)
            .map_err(|err| Error::Corrupt(format!("fast WAL tombstone: {err}")))?;
        let key_generation =
            cursor.u32().ok_or_else(|| Error::Corrupt("fast WAL tombstone key_gen".into()))?;
        let value =
            cursor.u64().ok_or_else(|| Error::Corrupt("fast WAL tombstone value".into()))?;
        let tag = cursor
            .array::<TAG_LEN>()
            .ok_or_else(|| Error::Corrupt("fast WAL tombstone tag".into()))?;
        let tombstone = match magic {
            EMPTY_FENCE_MAGIC => Tombstone::EmptyFence { by_epoch: value },
            ROLLED_TOMBSTONE_MAGIC => Tombstone::Rolled { max_commit_epoch: value },
            SWEPT_TOMBSTONE_MAGIC => Tombstone::Swept { head_epoch: value },
            _ => return Err(Error::Corrupt("fast WAL bad tombstone magic".into())),
        };
        let aead = derive_tombstone_aead(
            expected_cell,
            expected_epoch,
            expected_segment,
            keys,
            key_generation,
            magic,
            value,
        )?;
        let aad = tombstone_aad(
            magic,
            version,
            key_generation,
            expected_cell,
            expected_epoch,
            expected_segment,
            value,
        );
        let mut empty = [];
        aead.open(0, &aad, &mut empty, &tag)
            .map_err(|err| Error::Corrupt(format!("fast WAL tombstone auth: {err}")))?;
        Ok(tombstone)
    }

    fn derive_aead_with(
        header: &WalHeader,
        keys: &Ring,
        label: &[u8],
        extra: Option<&[u8]>,
    ) -> Result<Aead, Error> {
        let key_id = KeyId { cell: header.cell.id, generation: header.key_generation };
        let cell_keys = keys.of(header.cell.id);
        let dek = cell_keys.dek(key_id).ok_or(Error::NoKey(key_id))?;
        let tenant = header.cell.tenant.to_le_bytes();
        let cell_id = header.cell.id.to_le_bytes();
        let epoch = header.epoch.to_le_bytes();
        let segment = header.segment.to_le_bytes();
        let subkey = match extra {
            Some(extra) => {
                dek.derive(&header.salt, &[label, &tenant, &cell_id, &epoch, &segment, extra])
            }
            None => dek.derive(&header.salt, &[label, &tenant, &cell_id, &epoch, &segment]),
        };
        Ok(Aead::new(&subkey))
    }

    pub(super) fn derive_aead(header: &WalHeader, keys: &Ring) -> Result<Aead, Error> {
        derive_aead_with(header, keys, b"cladefs-fast-wal", None)
    }

    /// Derives a domain-separated AEAD key for a taker's `Fence` record at `by_epoch`, guaranteeing
    /// zero AES-GCM nonce reuse with concurrent writer records or other takers.
    pub(super) fn derive_fence_aead(
        header: &WalHeader,
        keys: &Ring,
        by_epoch: u64,
    ) -> Result<Aead, Error> {
        let by = by_epoch.to_le_bytes();
        derive_aead_with(header, keys, b"cladefs-fast-wal-fence", Some(&by))
    }

    fn record_aad(header_bytes: &[u8; HEADER_LEN], seq: u64, payload_len: u32) -> [u8; 92] {
        let mut aad = Vec::with_capacity(92);
        aad.extend_from_slice(header_bytes);
        aad.extend_from_slice(&seq.to_le_bytes());
        aad.extend_from_slice(&payload_len.to_le_bytes());
        aad.try_into().expect("record AAD is 92 bytes")
    }

    fn fence_aad(header_bytes: &[u8; HEADER_LEN], rec_offset: u64, by_epoch: u64) -> [u8; 96] {
        let mut aad = Vec::with_capacity(96);
        aad.extend_from_slice(header_bytes);
        aad.extend_from_slice(&rec_offset.to_le_bytes());
        aad.extend_from_slice(&by_epoch.to_le_bytes());
        aad.try_into().expect("fence AAD is 96 bytes")
    }

    /// Encodes an authenticated `CFWF` fence record at exact segment byte offset `rec_offset`.
    pub(super) fn encode_fence(
        header: &WalHeader,
        header_bytes: &[u8; HEADER_LEN],
        keys: &Ring,
        rec_offset: u64,
        by_epoch: u64,
    ) -> Result<Vec<u8>, Error> {
        let aead = derive_fence_aead(header, keys, by_epoch)?;
        let aad = fence_aad(header_bytes, rec_offset, by_epoch);
        let mut empty = [];
        let tag = aead.seal(rec_offset, &aad, &mut empty);
        let total = 4usize
            .checked_add(8)
            .and_then(|n| n.checked_add(TAG_LEN))
            .ok_or_else(|| Error::Corrupt("fast WAL fence size overflow".into()))?;
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&FENCE_MAGIC);
        out.extend_from_slice(&by_epoch.to_le_bytes());
        out.extend_from_slice(&tag);
        Ok(out)
    }

    pub(super) fn encode_record(
        header_bytes: &[u8; HEADER_LEN],
        aead: &Aead,
        seq: u64,
        record: &WalRecord,
    ) -> Result<Vec<u8>, Error> {
        if matches!(record, WalRecord::Fence { .. }) {
            return Err(Error::Corrupt("fence records must use encode_fence".into()));
        }
        let mut payload = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .serialize(record)
            .map_err(|err| Error::Corrupt(format!("fast WAL serialize: {err}")))?;
        if payload.len() > MAX_RECORD_PAYLOAD {
            return Err(Error::Corrupt(format!(
                "fast WAL record exceeds limit: {} bytes",
                payload.len()
            )));
        }
        let len = u32::try_from(payload.len()).map_err(|_| {
            Error::Corrupt(format!("fast WAL record exceeds limit: {} bytes", payload.len()))
        })?;
        let aad = record_aad(header_bytes, seq, len);
        let tag = aead.seal(seq, &aad, &mut payload);
        let total = 4usize
            .checked_add(8)
            .and_then(|n| n.checked_add(4))
            .and_then(|n| n.checked_add(payload.len()))
            .and_then(|n| n.checked_add(TAG_LEN))
            .ok_or_else(|| Error::Corrupt("fast WAL record size overflow".into()))?;
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&RECORD_MAGIC);
        out.extend_from_slice(&seq.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&payload);
        out.extend_from_slice(&tag);
        Ok(out)
    }

    pub(super) fn decode_header(
        bytes: &[u8],
        expected_cell: CellId,
        expected_epoch: u64,
        expected_segment: u64,
    ) -> Result<(WalHeader, [u8; HEADER_LEN]), Error> {
        let mut cursor = Cursor::new(bytes);
        let raw_header = cursor
            .array::<HEADER_LEN>()
            .ok_or_else(|| Error::Corrupt("fast WAL segment shorter than header".into()))?;
        let mut hdr = Cursor::new(&raw_header);
        let magic =
            hdr.array::<8>().ok_or_else(|| Error::Corrupt("fast WAL header magic".into()))?;
        if magic != HEADER_MAGIC {
            return Err(Error::Corrupt("fast WAL bad header magic".into()));
        }
        let version = hdr.u32().ok_or_else(|| Error::Corrupt("fast WAL version".into()))?;
        FORMAT.check(version).map_err(|err| Error::Corrupt(format!("fast WAL header: {err}")))?;
        let key_generation = hdr.u32().ok_or_else(|| Error::Corrupt("fast WAL key_gen".into()))?;
        let tenant = hdr.u64().ok_or_else(|| Error::Corrupt("fast WAL tenant".into()))?;
        let id = hdr.u64().ok_or_else(|| Error::Corrupt("fast WAL cell id".into()))?;
        let epoch = hdr.u64().ok_or_else(|| Error::Corrupt("fast WAL epoch".into()))?;
        let segment = hdr.u64().ok_or_else(|| Error::Corrupt("fast WAL segment".into()))?;
        let salt = Salt(hdr.array::<32>().ok_or_else(|| Error::Corrupt("fast WAL salt".into()))?);
        let cell = CellId { tenant, id };
        if cell != expected_cell || epoch != expected_epoch || segment != expected_segment {
            return Err(Error::Corrupt(format!(
                "fast WAL header mismatch: got ({cell}, ep={epoch}, seg={segment}), \
                 expected ({expected_cell}, ep={expected_epoch}, seg={expected_segment})"
            )));
        }
        Ok((WalHeader { key_generation, cell, epoch, segment, salt }, raw_header))
    }

    pub(super) type DecodedSegment = (WalHeader, [u8; HEADER_LEN], Vec<(u64, WalRecord)>);

    pub(super) fn decode_segment(
        bytes: &[u8],
        expected_cell: CellId,
        expected_epoch: u64,
        expected_segment: u64,
        keys: &Ring,
    ) -> Result<DecodedSegment, Error> {
        let (header, header_bytes) =
            decode_header(bytes, expected_cell, expected_epoch, expected_segment)?;
        let aead = derive_aead(&header, keys)?;
        let rest = bytes
            .get(HEADER_LEN..)
            .ok_or_else(|| Error::Corrupt("fast WAL missing body".into()))?;
        let mut cursor = Cursor::new(rest);
        let mut records = Vec::new();
        let mut min_next_seq = 0u64;
        let mut consumed = HEADER_LEN as u64;
        while let Some(magic) = cursor.array::<4>() {
            let rec_offset = consumed;
            if magic == FENCE_MAGIC {
                let by_epoch = cursor
                    .u64()
                    .ok_or_else(|| Error::Corrupt("fast WAL truncated fence epoch".into()))?;
                if by_epoch <= header.epoch {
                    return Err(Error::Corrupt(format!(
                        "fast WAL fence epoch {by_epoch} <= segment epoch {}",
                        header.epoch
                    )));
                }
                let tag = cursor
                    .array::<TAG_LEN>()
                    .ok_or_else(|| Error::Corrupt("fast WAL truncated fence tag".into()))?;
                let fence_aead = derive_fence_aead(&header, keys, by_epoch)?;
                let aad = fence_aad(&header_bytes, rec_offset, by_epoch);
                let mut empty = [];
                fence_aead
                    .open(rec_offset, &aad, &mut empty, &tag)
                    .map_err(|err| Error::Corrupt(format!("fast WAL fence: {err}")))?;
                records.push((u64::MAX, WalRecord::Fence { by_epoch }));
                break;
            }
            if magic != RECORD_MAGIC {
                return Err(Error::Corrupt("fast WAL bad record magic".into()));
            }
            let seq =
                cursor.u64().ok_or_else(|| Error::Corrupt("fast WAL truncated seq".into()))?;
            if seq < min_next_seq {
                return Err(Error::Corrupt(format!(
                    "fast WAL non-monotonic record seq {seq}, expected >= {min_next_seq}"
                )));
            }
            let len =
                cursor.u32().ok_or_else(|| Error::Corrupt("fast WAL truncated len".into()))?;
            if (len as usize) > MAX_RECORD_PAYLOAD {
                return Err(Error::Corrupt(format!("fast WAL record too large: {len}")));
            }
            let ciphertext = cursor
                .take(len as usize)
                .ok_or_else(|| Error::Corrupt("fast WAL truncated ciphertext".into()))?;
            let tag = cursor
                .array::<TAG_LEN>()
                .ok_or_else(|| Error::Corrupt("fast WAL truncated tag".into()))?;
            let aad = record_aad(&header_bytes, seq, len);
            let mut plaintext = ciphertext.to_vec();
            aead.open(seq, &aad, &mut plaintext, &tag)
                .map_err(|err| Error::Corrupt(format!("fast WAL record {seq}: {err}")))?;
            let record: WalRecord = bincode::DefaultOptions::new()
                .with_fixint_encoding()
                .with_limit(MAX_RECORD_PAYLOAD as u64)
                .reject_trailing_bytes()
                .deserialize(&plaintext)
                .map_err(|err| Error::Corrupt(format!("fast WAL decode record {seq}: {err}")))?;
            if matches!(record, WalRecord::Fence { .. }) {
                return Err(Error::Corrupt("fast WAL unexpected fence in writer record".into()));
            }
            let rec_len = 4u64
                .checked_add(8)
                .and_then(|n| n.checked_add(4))
                .and_then(|n| n.checked_add(u64::from(len)))
                .and_then(|n| n.checked_add(TAG_LEN as u64))
                .ok_or_else(|| Error::Corrupt("fast WAL record offset overflow".into()))?;
            consumed = consumed
                .checked_add(rec_len)
                .ok_or_else(|| Error::Corrupt("fast WAL segment offset overflow".into()))?;
            records.push((seq, record));
            min_next_seq = seq
                .checked_add(1)
                .ok_or_else(|| Error::Corrupt("fast WAL sequence overflow".into()))?;
        }
        if cursor.take(1).is_some() {
            return Err(Error::Corrupt("fast WAL trailing bytes after last record".into()));
        }
        Ok((header, header_bytes, records))
    }
}

/// Adaptive group-commit batching policy (doc-29):
/// - 0 ms linger when serial/idle (queue empty and no recent concurrency).
/// - Natural coalescing while an append is in flight.
/// - Short EWMA-RTT adaptive linger (`(rtt / 4).clamp(250µs, 2ms)`) when concurrent commits are
///   arriving, immediately returning to 0 ms as soon as concurrency subsides.
#[derive(Clone, Debug)]
pub struct AdaptiveBatcher {
    ewma_rtt_us: u64,
    recent_concurrency: bool,
}

impl Default for AdaptiveBatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl AdaptiveBatcher {
    pub fn new() -> Self {
        Self { ewma_rtt_us: 2_000, recent_concurrency: false }
    }

    /// Computes how long to linger when `queued_commits` commits totaling `queued_bytes` are ready
    /// to send right now.
    pub fn linger(&self, queued_commits: usize, queued_bytes: usize) -> Duration {
        if queued_commits == 0
            || queued_commits >= MAX_BATCH_COMMITS
            || queued_bytes >= MAX_BATCH_BYTES
        {
            return Duration::ZERO;
        }
        if !self.recent_concurrency {
            return Duration::ZERO;
        }
        let target_us = (self.ewma_rtt_us / 4)
            .clamp(MIN_LINGER.as_micros() as u64, MAX_LINGER.as_micros() as u64);
        Duration::from_micros(target_us)
    }

    /// Updates state after a batch of `batch_commits` completes with round-trip `rtt`.
    pub fn observe(&mut self, batch_commits: usize, rtt: Duration) {
        let sample_us = (rtt.as_micros() as u64).max(100);
        // EWMA with alpha = 1/4.
        self.ewma_rtt_us = (self.ewma_rtt_us * 3 + sample_us) / 4;
        self.recent_concurrency = batch_commits > 1;
    }

    /// Marks whether concurrency was observed (e.g. multiple commits queued while an append was in
    /// flight).
    pub fn note_queued(&mut self, queued_commits: usize) {
        if queued_commits > 1 {
            self.recent_concurrency = true;
        }
    }
}

struct SinkState {
    queue: VecDeque<(StagedCommit, Holding)>,
    queued_bytes: usize,
    failed: Option<Failure>,
    stopped: bool,
}

/// Shared handle connecting `LocalCell::flush_locked` (producer), `Uploader::fast_wal` (waiter),
/// `Shared::acknowledge_pending` (head-ack notifier), and the background [`FastWalWriter`] task.
pub struct FastWalSink {
    bucket_url: String,
    durable: AtomicU64,
    head_acked: AtomicU64,
    degraded: AtomicBool,
    state: Mutex<SinkState>,
    /// Wakes the background fast WAL writer task.
    wake_writer: tokio::sync::Notify,
    /// Shared with `upload::Shared::changed` so `Uploader::fast_wal` waiters wake immediately.
    changed: Arc<Event>,
    /// Urges the cold uploader when the fast bucket is degraded.
    urge_cold: Arc<dyn Fn() + Send + Sync>,
    /// Host governor memory accounting (`HotBudget`, board 12331): reports every staged commit's
    /// heap bytes to the daemon governor without holding request slots on `Writer`'s upload
    /// `Budget` semaphore (which would deadlock `put_large_extent` / cold uploads).
    hot_budget: Option<Arc<HotBudget>>,
}

impl FastWalSink {
    pub(crate) fn new(
        bucket_url: String,
        initial_epoch: u64,
        changed: Arc<Event>,
        urge_cold: Arc<dyn Fn() + Send + Sync>,
        hot_budget: Option<Arc<HotBudget>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            bucket_url,
            durable: AtomicU64::new(initial_epoch),
            head_acked: AtomicU64::new(initial_epoch),
            degraded: AtomicBool::new(false),
            state: Mutex::new(SinkState {
                queue: VecDeque::new(),
                queued_bytes: 0,
                failed: None,
                stopped: false,
            }),
            wake_writer: tokio::sync::Notify::new(),
            changed,
            urge_cold,
            hot_budget,
        })
    }

    /// Stages commits in strict commit order from `LocalCell::flush_locked` (zero disk fsync),
    /// reporting their heap bytes to the host memory governor via [`Holding`].
    pub fn push(&self, commits: Vec<StagedCommit>, _last: u64) {
        if commits.is_empty() {
            return;
        }
        {
            let mut state = self.state.lock().unwrap();
            for commit in commits {
                let mut holding = Holding::on(self.hot_budget.clone());
                holding.hold(commit.staged_heap_bytes());
                state.queued_bytes =
                    state.queued_bytes.saturating_add(commit.estimated_wal_bytes());
                state.queue.push_back((commit, holding));
            }
        }
        if self.degraded.load(Ordering::Acquire) {
            (self.urge_cold)();
        }
        self.wake_writer.notify_one();
    }

    /// Last commit epoch acknowledged as durable in the fast WAL (or head-acknowledged in cold).
    pub fn durable(&self) -> u64 {
        self.durable.load(Ordering::Acquire)
    }

    /// Whether the fast WAL bucket is currently degraded and commits are falling back to cold
    /// regional upload.
    pub fn degraded(&self) -> bool {
        self.degraded.load(Ordering::Acquire)
    }

    /// Bucket URL of this sink's active fast WAL.
    pub fn bucket_url(&self) -> &str {
        &self.bucket_url
    }

    /// Current live status for `StatCell`.
    pub fn status(&self) -> FastWalStatus {
        FastWalStatus {
            bucket: self.bucket_url.clone(),
            degraded: self.degraded(),
            durable: self.durable(),
        }
    }

    /// Terminal failure of the fast WAL writer (e.g., fenced by takeover), if any.
    pub fn failed(&self) -> Option<Failure> {
        self.state.lock().unwrap().failed.clone()
    }

    /// Highest commit epoch acknowledged in the cold regional bucket head.
    pub fn head_acked(&self) -> u64 {
        self.head_acked.load(Ordering::Acquire)
    }

    /// Notified by the cold uploader whenever the cold bucket head advances to `epoch`.
    /// Lets the fast WAL writer reclaim rolled WAL segments (`rolledTombstoneOnDelete`) and drop
    /// queued commits already covered by the cold head during fast-bucket outages.
    pub fn head_acknowledged(&self, epoch: u64) {
        let prev = self.head_acked.fetch_max(epoch, Ordering::AcqRel);
        if epoch > prev {
            self.durable.fetch_max(epoch, Ordering::AcqRel);
            self.wake_writer.notify_one();
            self.changed.notify(usize::MAX);
        }
    }

    /// Stops the background fast WAL writer task.
    pub fn stop(&self) {
        self.state.lock().unwrap().stopped = true;
        self.wake_writer.notify_one();
        self.changed.notify(usize::MAX);
    }

    fn mark_durable(&self, epoch: u64) {
        self.durable.fetch_max(epoch, Ordering::AcqRel);
        self.changed.notify(usize::MAX);
    }

    fn set_degraded(&self, degraded: bool) {
        let prev = self.degraded.swap(degraded, Ordering::AcqRel);
        if degraded && !prev {
            log::warn!(
                "cladefs: fast WAL bucket {} degraded; urging cold regional bucket uploads",
                self.bucket_url
            );
            (self.urge_cold)();
        } else if !degraded && prev {
            log::info!("cladefs: fast WAL bucket {} recovered", self.bucket_url);
        }
    }

    fn fail(&self, failure: Failure) {
        {
            let mut state = self.state.lock().unwrap();
            state.failed.get_or_insert(failure);
            state.stopped = true;
        }
        self.changed.notify(usize::MAX);
    }
}

/// Active fast WAL segment cursor maintained by [`FastWalWriter`].
struct OpenSegment {
    segment: u64,
    offset: u64,
    appends: u32,
    next_record_seq: u64,
    max_commit_epoch: u64,
    header_bytes: [u8; HEADER_LEN],
    aead: Aead,
}

impl OpenSegment {
    fn new(
        cell: CellId,
        epoch: u64,
        segment: u64,
        keys: &Ring,
    ) -> Result<Self, bucket_cell::Error> {
        let (_, key_id) = keys.try_encrypting()?;
        let header = WalHeader {
            key_generation: key_id.generation,
            cell,
            epoch,
            segment,
            salt: Salt::random(),
        };
        let header_bytes = wire::encode_header(&header);
        let aead = wire::derive_aead(&header, keys)?;
        Ok(Self {
            segment,
            offset: 0,
            appends: 0,
            next_record_seq: 0,
            max_commit_epoch: 0,
            header_bytes,
            aead,
        })
    }
}

/// Background fast WAL writer task owning append sequencing, adaptive batching, large-extent
/// pre-upload, proactive rollover, and rolled-segment tombstone reclamation.
pub struct FastWalWriter<C: Bucket, F: Bucket> {
    cell: CellId,
    epoch: u64,
    cold_writer: Arc<Writer<C>>,
    fast_bucket: Arc<F>,
    sink: Arc<FastWalSink>,
    max_writer_appends: u32,
    max_segment_bytes: u64,
    batcher: AdaptiveBatcher,
    open: Option<OpenSegment>,
    next_segment: u64,
    /// Rolled segments awaiting `head_acked >= max_commit_epoch` before overwriting with
    /// `ROLLED_TOMBSTONE` (`noDeleteBeforeAck`, `rolledTombstoneOnDelete`).
    rolled: VecDeque<(u64, u64)>,
    /// Consecutive failures reclaiming a rolled segment, used for exponential retry backoff.
    reclaim_failures: u32,
    /// The back-off after a failed reclaim, while it runs: no reclaim is tried until it ends.
    reclaim_wait: Option<BoxFuture<'static, ()>>,
    /// What lingers and back-offs wait on: the uploader's clock, the host runtime's sleep in
    /// production and the world's in the simulation (board 14433).
    clock: Clock,
}

impl<C: Bucket, F: Bucket> FastWalWriter<C, F> {
    pub(crate) fn new(
        cold_writer: Arc<Writer<C>>,
        fast_bucket: Arc<F>,
        sink: Arc<FastWalSink>,
        max_writer_appends: u32,
        max_segment_bytes: u64,
        clock: Clock,
    ) -> Self {
        let cell = cold_writer.cell();
        let epoch = cold_writer.epoch();
        Self {
            cell,
            epoch,
            cold_writer,
            fast_bucket,
            sink,
            max_writer_appends: max_writer_appends.max(1),
            max_segment_bytes: max_segment_bytes.max(HEADER_LEN as u64 + 4096),
            batcher: AdaptiveBatcher::new(),
            open: None,
            next_segment: 0,
            rolled: VecDeque::new(),
            reclaim_failures: 0,
            reclaim_wait: None,
            clock,
        }
    }

    /// Runs the writer loop until stopped or fenced.
    pub async fn run(mut self) {
        loop {
            self.reclaim_rolled().await;
            let (batch, stopped) = self.take_batch().await;
            if batch.is_empty() {
                if stopped {
                    break;
                }
                continue;
            }
            if let Err(err) = self.flush_batch(batch).await {
                self.sink.fail(err);
                break;
            }
        }
        self.reclaim_rolled().await;
    }

    fn rolled_ready(&self) -> bool {
        let acked = self.sink.head_acked.load(Ordering::Acquire);
        self.rolled.front().is_some_and(|&(_, max_epoch)| reclaimable(max_epoch, acked))
    }

    /// Whether a back-off after a failed reclaim still runs, dropping it once it ended.
    fn reclaim_backing_off(&mut self) -> bool {
        let Some(wait) = self.reclaim_wait.as_mut() else { return false };
        if wait.as_mut().now_or_never().is_none() {
            return true;
        }
        self.reclaim_wait = None;
        false
    }

    fn has_reclaimable_rolled(&mut self) -> bool {
        self.rolled_ready() && !self.reclaim_backing_off()
    }

    /// Waits for staged commits and applies [`AdaptiveBatcher`]'s linger window.
    async fn take_batch(&mut self) -> (Vec<(StagedCommit, Holding)>, bool) {
        loop {
            // Register wake listener before inspecting state so no push is missed.
            let sink = self.sink.clone();
            let notified = sink.wake_writer.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let head_acked = self.sink.head_acked.load(Ordering::Acquire);
            let (count, bytes, stopped) = {
                let mut state = self.sink.state.lock().unwrap();
                // Drop leading commits already acknowledged in the cold bucket head
                // (maintains `walContiguous`: never skips an unacked commit).
                while state.queue.front().is_some_and(|(c, _)| c.epoch <= head_acked) {
                    if let Some((dropped, _slot)) = state.queue.pop_front() {
                        state.queued_bytes =
                            state.queued_bytes.saturating_sub(dropped.estimated_wal_bytes());
                    }
                }
                (state.queue.len(), state.queued_bytes, state.stopped)
            };
            if count == 0 {
                if stopped {
                    return (Vec::new(), true);
                }
                // Reclaim rolled segments immediately when cold acknowledgement wakes an idle writer,
                // respecting backoff if a previous reclaim overwrite failed.
                if self.has_reclaimable_rolled() {
                    return (Vec::new(), false);
                }
                if self.rolled_ready()
                    && let Some(wait) = self.reclaim_wait.as_mut()
                {
                    // Either a new wakeup or the back-off ended; the loop checks again.
                    if let Either::Right(_) = futures::future::select(notified, wait.as_mut()).await
                    {
                        self.reclaim_wait = None;
                    }
                    continue;
                }
                notified.await;
                continue;
            }
            self.batcher.note_queued(count);
            let linger = if stopped { Duration::ZERO } else { self.batcher.linger(count, bytes) };
            if !linger.is_zero() {
                let mut sleep = (self.clock)(linger);
                loop {
                    let wake = self.sink.wake_writer.notified();
                    tokio::pin!(wake);
                    wake.as_mut().enable();
                    let ready = {
                        let state = self.sink.state.lock().unwrap();
                        state.stopped
                            || state.queue.len() >= MAX_BATCH_COMMITS
                            || state.queued_bytes >= MAX_BATCH_BYTES
                    };
                    if ready {
                        break;
                    }
                    match futures::future::select(wake, sleep.as_mut()).await {
                        Either::Left(_) => continue,
                        Either::Right(_) => break,
                    }
                }
            }
            let mut state = self.sink.state.lock().unwrap();
            let mut batch = Vec::new();
            let mut batch_bytes = 0usize;
            while let Some((front, _)) = state.queue.front() {
                let est = front.estimated_wal_bytes();
                if !batch.is_empty()
                    && (batch.len() >= MAX_BATCH_COMMITS || batch_bytes + est > MAX_BATCH_BYTES)
                {
                    break;
                }
                let item = state.queue.pop_front().unwrap();
                state.queued_bytes = state.queued_bytes.saturating_sub(est);
                batch_bytes = batch_bytes.saturating_add(est);
                batch.push(item);
            }
            return (batch, state.stopped);
        }
    }

    async fn flush_batch(&mut self, batch: Vec<(StagedCommit, Holding)>) -> Result<(), Failure> {
        let batch_len = batch.len();
        let (mut commits, _holdings): (Vec<StagedCommit>, Vec<Holding>) = batch.into_iter().unzip();
        let last_epoch = commits.last().map_or(0, |c| c.epoch);

        // Step 1 (`largeBeforeWal`): move (never clone) all large own-extents straight to the cold
        // regional bucket before writing their metadata-only `LargeRef`s to the fast WAL.
        let mut large_uploads = Vec::new();
        for commit in &mut commits {
            for large in &mut commit.large_extents {
                let key = layout::extent(
                    self.cell,
                    self.epoch,
                    large.bucket_segment,
                    ExtentId::new(self.cell.id, large.sequence),
                );
                large_uploads.push((key, std::mem::take(&mut large.framed_bytes)));
            }
        }
        if rules().wal_after_large {
            put_large(&self.cold_writer, std::mem::take(&mut large_uploads)).await?;
        }

        let wire_commits: Vec<FastWalCommit> =
            commits.into_iter().map(StagedCommit::into_wire_commit).collect();
        let record = WalRecord::Batch(wire_commits);

        // If a single valid commit exceeds MAX_RECORD_PAYLOAD in inline extents, spill it cleanly
        // through the cold regional bucket uploader rather than failing the fast WAL writer.
        let approx_record_len = bincode::serialized_size(&record).unwrap_or(u64::MAX);
        if approx_record_len > MAX_RECORD_PAYLOAD as u64 {
            log::warn!(
                "cladefs: cell {} commit batch ({approx_record_len} bytes) exceeds fast WAL record \
                 limit; urging cold bucket upload",
                self.cell
            );
            (self.sink.urge_cold)();
            loop {
                let notified = self.sink.wake_writer.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.sink.head_acked.load(Ordering::Acquire) >= last_epoch
                    || self.sink.state.lock().unwrap().stopped
                {
                    return Ok(());
                }
                notified.await;
            }
        }

        // Step 2: check if the current WAL segment must proactively roll (`rollBeforeLimit`).
        if let Some(open) = &self.open {
            let hit_parts =
                rules().wal_rolls_before_limit && open.appends >= self.max_writer_appends;
            let hit_bytes = open.offset > 0
                && open.offset.saturating_add(approx_record_len).saturating_add(64)
                    > self.max_segment_bytes;
            if hit_parts || hit_bytes {
                self.roll_current_segment().await?;
            }
        }

        // Step 3: ensure an open segment exists, encode record, and append at exact offset.
        if self.open.is_none() {
            let seg = OpenSegment::new(
                self.cell,
                self.epoch,
                self.next_segment,
                self.cold_writer.keys(),
            )?;
            self.next_segment += 1;
            self.open = Some(seg);
        }

        let started = Instant::now();
        let landed = self.append_record(&record, last_epoch).await?;
        put_large(&self.cold_writer, large_uploads).await?;
        if landed {
            let rtt = started.elapsed();
            self.batcher.observe(batch_len, rtt);
            self.sink.set_degraded(false);
            self.sink.mark_durable(last_epoch);
        }
        Ok(())
    }

    /// Appends `WalRecord::Rolled` to the active segment (`rollBeforeNext`), then queues it for
    /// tombstone reclamation once the cold bucket head acknowledges `max_commit_epoch`.
    async fn roll_current_segment(&mut self) -> Result<(), Failure> {
        let Some(open) = &self.open else { return Ok(()) };
        let seg_num = open.segment;
        let max_epoch = open.max_commit_epoch;
        let rolled = match rules().wal_rolls_before_next {
            true => self.append_record(&WalRecord::Rolled, max_epoch).await?,
            false => true,
        };
        if rolled {
            self.open = None;
            self.rolled.push_back((seg_num, max_epoch));
        }
        Ok(())
    }

    /// Appends `record` to `self.open`, retrying transient fast-bucket failures while urging the
    /// cold uploader (degraded mode). Returns `Ok(true)` once appended, `Ok(false)` if the cold
    /// head already acknowledged past `last_epoch` while degraded, or `Err(Fenced)` if fenced.
    async fn append_record(
        &mut self,
        record: &WalRecord,
        last_epoch: u64,
    ) -> Result<bool, Failure> {
        let open = self.open.as_mut().expect("open segment");
        let key = layout::wal(self.cell, self.epoch, open.segment);
        let mut transient_failures = 0u32;

        // Mint a strictly monotonic `seq` BEFORE sealing so a timed-out append abandoned after
        // cold-head acknowledgement can NEVER share an AES-GCM nonce or sequence with a later
        // batch.
        let mut seq = open.next_record_seq;
        open.next_record_seq = open.next_record_seq.saturating_add(1);
        let mut offset = open.offset;
        let encoded_record = wire::encode_record(&open.header_bytes, &open.aead, seq, record)?;
        let mut payload = if offset == 0 {
            let mut combined = Vec::with_capacity(HEADER_LEN + encoded_record.len());
            combined.extend_from_slice(&open.header_bytes);
            combined.extend_from_slice(&encoded_record);
            Bytes::from(combined)
        } else {
            Bytes::from(encoded_record)
        };

        loop {
            match self.fast_bucket.append(&key, offset, payload.clone()).await {
                Ok(new_len) => {
                    open.offset = new_len;
                    open.appends = open.appends.saturating_add(1);
                    open.max_commit_epoch = open.max_commit_epoch.max(last_epoch);
                    return Ok(true);
                }
                Err(bucket::Error::Precondition | bucket::Error::AlreadyExists)
                    if !rules().wal_offset_checked =>
                {
                    // Re-reads where the object ends, and appends there, or creates it again.
                    let encoded = wire::encode_record(&open.header_bytes, &open.aead, seq, record)?;
                    (offset, payload) = match bucket::head(&*self.fast_bucket, &key).await? {
                        Some(meta) => (meta.size, Bytes::from(encoded)),
                        None => (0, Bytes::from([&open.header_bytes[..], &encoded].concat())),
                    };
                }
                Err(bucket::Error::Precondition | bucket::Error::AlreadyExists) => {
                    // Either:
                    // (a) a taker fenced this segment (tombstone or `Fence`),
                    // (b) a previous retry of THIS exact record already landed and lost its reply,
                    // or (c) an earlier timed-out batch (dropped after cold-head ack while
                    // degraded) landed late and advanced the server offset.
                    match bucket::get(&*self.fast_bucket, &key).await {
                        Ok(Some(obj)) => {
                            if obj.bytes.len() == TOMBSTONE_LEN {
                                return Err(bucket_cell::Error::Fenced.into());
                            }
                            if let Ok((_, _, recs)) = wire::decode_segment(
                                &obj.bytes,
                                self.cell,
                                self.epoch,
                                open.segment,
                                self.cold_writer.keys(),
                            ) {
                                if recs.iter().any(|(_, r)| matches!(r, WalRecord::Fence { .. })) {
                                    return Err(bucket_cell::Error::Fenced.into());
                                }
                                // Verify BOTH sequence AND exact record contents so a late
                                // earlier batch can never falsely acknowledge a newer batch!
                                if recs.iter().any(|(r_seq, r)| *r_seq == seq && r == record) {
                                    open.offset = obj.bytes.len() as u64;
                                    open.appends = open.appends.max(recs.len() as u32);
                                    open.max_commit_epoch = open.max_commit_epoch.max(last_epoch);
                                    return Ok(true);
                                }
                                // If the segment is not rolled (and not fenced), an earlier
                                // timed-out batch of our own epoch landed late. Reconcile our
                                // cursor and retry with a fresh monotonic sequence number!
                                if !recs.iter().any(|(_, r)| matches!(r, WalRecord::Rolled))
                                    && (obj.bytes.len() as u64) > offset
                                {
                                    open.offset = obj.bytes.len() as u64;
                                    open.appends = open.appends.max(recs.len() as u32);
                                    let max_seen = recs.iter().map(|(s, _)| *s).max().unwrap_or(0);
                                    open.next_record_seq =
                                        open.next_record_seq.max(max_seen.saturating_add(1));
                                    seq = open.next_record_seq;
                                    open.next_record_seq = open.next_record_seq.saturating_add(1);
                                    offset = open.offset;
                                    payload = Bytes::from(wire::encode_record(
                                        &open.header_bytes,
                                        &open.aead,
                                        seq,
                                        record,
                                    )?);
                                    continue;
                                }
                            }
                            return Err(bucket_cell::Error::Fenced.into());
                        }
                        Ok(None) => return Err(bucket_cell::Error::Fenced.into()),
                        Err(refused) => return Err(refused.into()),
                    }
                }
                Err(bucket::Error::Unsupported) => {
                    return Err(bucket::Refused { key, refusal: bucket::Refusal::Request }.into());
                }
                Err(bucket::Error::Refused(refusal)) => {
                    return Err(bucket::Refused { key, refusal }.into());
                }
                Err(
                    bucket::Error::Unavailable | bucket::Error::Expired | bucket::Error::NotFound,
                ) => {
                    transient_failures = transient_failures.saturating_add(1);
                    self.sink.set_degraded(true);
                    (self.sink.urge_cold)();
                    // If the cold bucket already acknowledged these commits (and this isn't a
                    // `Rolled` marker we need to land), stop waiting on the degraded fast bucket
                    // for this batch (`walContiguous`: we drop the whole batch only if covered).
                    if !matches!(record, WalRecord::Rolled)
                        && (!rules().wal_contiguous
                            || self.sink.head_acked.load(Ordering::Acquire) >= last_epoch)
                    {
                        return Ok(false);
                    }
                    if self.sink.state.lock().unwrap().stopped {
                        return Ok(false);
                    }
                    self.fast_bucket.pause(transient_failures).await;
                }
            }
        }
    }

    /// Overwrites rolled segments whose commits are fully acknowledged in the cold bucket head
    /// with an authenticated 36-byte `CFTR` rolled tombstone carrying `max_commit_epoch`
    /// (`noDeleteBeforeAck`, `rolledTombstoneOnDelete`, board 12299).
    async fn reclaim_rolled(&mut self) {
        if self.reclaim_backing_off() {
            return;
        }
        let acked = self.sink.head_acked.load(Ordering::Acquire);
        while let Some(&(seg, max_epoch)) = self.rolled.front() {
            if !reclaimable(max_epoch, acked) {
                break;
            }
            let key = layout::wal(self.cell, self.epoch, seg);
            let tombstone = match wire::encode_tombstone(
                self.cell,
                self.epoch,
                seg,
                self.cold_writer.keys(),
                Tombstone::Rolled { max_commit_epoch: max_epoch },
            ) {
                Ok(bytes) => Bytes::from(bytes.to_vec()),
                Err(err) => {
                    log::warn!("cladefs: encoding rolled tombstone for {key}: {err}");
                    break;
                }
            };
            let reclaimed = match rules().wal_rolled_tombstones {
                true => self.fast_bucket.put(&key, tombstone, bucket::PutMode::Overwrite).await,
                false => self.fast_bucket.delete(&key).await.map(|()| bucket::Version::default()),
            };
            match reclaimed {
                Ok(_) => {
                    self.rolled.pop_front();
                    self.reclaim_failures = 0;
                }
                Err(err) => {
                    self.reclaim_failures = self.reclaim_failures.saturating_add(1);
                    let backoff = Duration::from_millis(100u64 << self.reclaim_failures.min(8));
                    self.reclaim_wait = Some((self.clock)(backoff));
                    log::warn!(
                        "cladefs: reclaiming rolled fast WAL segment {key}: {err} (retry in \
                         {backoff:?})"
                    );
                    break;
                }
            }
        }
    }
}

/// Uploads `uploads`, large own-extents framed under their keys, to the cold bucket by `writer`.
async fn put_large<C: Bucket>(
    writer: &Arc<Writer<C>>,
    uploads: Vec<(String, Vec<u8>)>,
) -> Result<(), Failure> {
    let results: Vec<Result<(), bucket_cell::Error>> = futures::stream::iter(uploads)
        .map(|(key, bytes)| async move { writer.put_large_extent(&key, bytes).await })
        .buffer_unordered(LARGE_PUTS_AT_ONCE)
        .collect()
        .await;
    for result in results {
        result?;
    }
    Ok(())
}

/// Whether a rolled segment whose commits reach `max_epoch` may be reclaimed, the cold bucket's
/// head having acknowledged through `acked` (`deleteWalAfterAck`).
fn reclaimable(max_epoch: u64, acked: u64) -> bool {
    max_epoch <= acked || !rules().wal_reclaims_after_ack
}

/// Fences every WAL segment of `old_active.epoch` in `old_active.bucket`, recovers all commits
/// past `writer.state().last_commit`, fetches & verifies any `LargeRef` own-extents from the cold
/// regional bucket, streams recovered cold segment(s) in bounded memory, and atomically switches
/// `head.fast_wal` to `target_fast_wal` in the same head CAS (`activateAfterFlush`,
/// `fenceFollowsRoll`).
pub async fn fence_and_recover<C: Bucket, F: Bucket>(
    writer: &Arc<Writer<C>>,
    cold_bucket: &Arc<C>,
    fast_bucket: &Arc<F>,
    old_active: &ActiveWal,
    target_fast_wal: Option<FastWal>,
) -> Result<usize, Failure> {
    let cell = writer.cell();
    let new_epoch = writer.epoch();
    let old_epoch = old_active.epoch;
    let keys = writer.keys();
    let encrypting = keys.try_encrypting()?;
    let mut chain = writer.chain();
    let mut builder = SegmentBuilder::default();
    let mut builder_bytes = 0usize;
    let mut last_uploaded_seg: Option<u64> = None;
    let mut last_seen_epoch = writer.state().last_commit.map_or(0, epoch_of);
    let mut recovered_count = 0usize;
    let mut seg = 0u64;
    // The segment and length at which a fence was last refused, and how many times in a row: a
    // refusal there with nobody appending meanwhile is a write in progress (S3's 409), which ends,
    // or the segment at its appends' limit, which lasts; refused `FENCE_REFUSALS` times, backing off
    // between, the take fails rather than try for ever (board 14433).
    let mut refused: Option<(u64, u64, u32)> = None;
    if !rules().wal_activates_after_flush {
        writer.activate_fast_wal(target_fast_wal.clone()).await?;
    }

    loop {
        let key = layout::wal(cell, old_epoch, seg);
        // Check object existence and size BEFORE downloading so an oversized object cannot
        // exhaust daemon memory (Hotbed review 118), and charge the download to the host budget.
        let head_meta = bucket::head(&**fast_bucket, &key).await?;
        let (obj_opt, _seg_slot) = match head_meta {
            None => (None, None),
            Some(meta) => {
                if meta.size > MAX_RECOVERY_WAL_OBJECT_BYTES {
                    return Err(bucket_cell::Error::Corrupt(format!(
                        "fast WAL segment {key} size {} exceeds recovery limit \
                         {MAX_RECOVERY_WAL_OBJECT_BYTES}",
                        meta.size
                    ))
                    .into());
                }
                let slot = writer.budget().hold(Priority::Demand, meta.size).await;
                (bucket::get(&**fast_bucket, &key).await?, Some(slot))
            }
        };
        let (seg_commits, done) = match obj_opt {
            None if !rules().wal_fences => (Vec::new(), true),
            None => {
                // Segment does not exist yet: fence it at offset 0 with an authenticated 36-byte
                // empty-segment fence tombstone (`CFTF`). If the old writer raced and created it,
                // re-read this segment.
                let tomb = wire::encode_tombstone(
                    cell,
                    old_epoch,
                    seg,
                    keys,
                    Tombstone::EmptyFence { by_epoch: new_epoch },
                )?;
                match bucket::append(&**fast_bucket, &key, 0, Bytes::from(tomb.to_vec())).await? {
                    Appended::Appended(_) => (Vec::new(), true),
                    Appended::Precondition => continue,
                }
            }
            Some(obj) if obj.bytes.len() == TOMBSTONE_LEN => {
                // Authenticate every tombstone before acting on it (board 12299).
                match wire::decode_tombstone(&obj.bytes, cell, old_epoch, seg, keys)? {
                    Tombstone::EmptyFence { by_epoch } => {
                        if by_epoch <= old_epoch || by_epoch > new_epoch {
                            return Err(bucket_cell::Error::Corrupt(format!(
                                "fast WAL empty fence tombstone on {key} has invalid epoch \
                                 {by_epoch} (old={old_epoch}, new={new_epoch})"
                            ))
                            .into());
                        }
                        (Vec::new(), true)
                    }
                    Tombstone::Rolled { max_commit_epoch } => {
                        if max_commit_epoch > last_seen_epoch {
                            return Err(bucket_cell::Error::Corrupt(format!(
                                "fast WAL rolled tombstone on {key} covers commit epoch \
                                 {max_commit_epoch}, past recovered epoch {last_seen_epoch}"
                            ))
                            .into());
                        }
                        if !rules().wal_fence_follows_roll {
                            (Vec::new(), true)
                        } else {
                            seg += 1;
                            continue;
                        }
                    }
                    Tombstone::Swept { head_epoch } => {
                        return Err(bucket_cell::Error::Corrupt(format!(
                            "fast WAL active segment {key} holds superseded sweep tombstone \
                             (head_epoch={head_epoch})"
                        ))
                        .into());
                    }
                }
            }
            Some(obj) => {
                let (header, header_bytes, records) =
                    wire::decode_segment(&obj.bytes, cell, old_epoch, seg, keys)?;
                let mut seg_commits = Vec::new();
                let mut ended_by_roll = false;
                let mut ended_by_fence = false;
                for (_, rec) in records {
                    match rec {
                        WalRecord::Batch(commits) => seg_commits.extend(commits),
                        WalRecord::Rolled => ended_by_roll = true,
                        WalRecord::Fence { .. } => ended_by_fence = true,
                    }
                }
                if ended_by_fence || !rules().wal_fences {
                    (seg_commits, true)
                } else if ended_by_roll {
                    (seg_commits, !rules().wal_fence_follows_roll)
                } else {
                    // Segment is still open: append domain-separated authenticated `CFWF` fence
                    // record at exact end offset (zero nonce reuse with concurrent writer).
                    let rec_offset = obj.bytes.len() as u64;
                    let fence_bytes =
                        wire::encode_fence(&header, &header_bytes, keys, rec_offset, new_epoch)?;
                    match bucket::append(&**fast_bucket, &key, rec_offset, Bytes::from(fence_bytes))
                        .await?
                    {
                        Appended::Appended(_) => (seg_commits, true),
                        // Old writer appended another batch or `Rolled`, or a write of its is in
                        // progress: re-read this segment.
                        Appended::Precondition => {
                            let times = match refused {
                                Some((at, end, times)) if (at, end) == (seg, rec_offset) => {
                                    times + 1
                                }
                                _ => 1,
                            };
                            if times >= FENCE_REFUSALS {
                                return Err(Failure::msg(format!(
                                    "fast WAL segment {key} refused a fence at its end, \
                                     {rec_offset}, {times} times, nobody appending: its appends' \
                                     limit reached, it can be fenced never"
                                )));
                            }
                            refused = Some((seg, rec_offset, times));
                            fast_bucket.pause(times).await;
                            continue;
                        }
                    }
                }
            }
        };

        // Release the WAL segment's raw-object budget slot now that `obj_opt` is consumed.
        drop(_seg_slot);

        // Stream this segment's missing commits into cold segments in bounded memory (board 12183):
        // inline extents are bounded by `RECOVERY_SEGMENT_BYTES`, and each `LargeRef` own-extent
        // is verified and streamed directly to `new_epoch`'s key ONE AT A TIME under `writer.budget()`
        // so recovery never accumulates multiple large extents in RAM.
        for commit in seg_commits {
            if commit.epoch <= last_seen_epoch {
                continue;
            }
            last_seen_epoch = commit.epoch;
            recovered_count += 1;

            let inline_bytes: usize = commit
                .inline_extents
                .iter()
                .map(|(_, b)| b.len())
                .sum::<usize>()
                .saturating_add(256);
            if !builder.is_empty() && builder_bytes + inline_bytes > RECOVERY_SEGMENT_BYTES {
                let objects = builder.close(&mut chain).frame(&encrypting);
                let seg_num = writer.add_segment(objects)?;
                writer.upload(seg_num).await?;
                last_uploaded_seg = Some(seg_num);
                builder = SegmentBuilder::default();
                builder_bytes = 0;
            }

            // `target_seg` is the exact cold segment number `builder.close(&mut chain)` will stamp
            // for this commit (we only cut segments before starting a commit, never mid-commit).
            let target_seg = chain.next_segment();
            let mut own_preframed = Vec::with_capacity(commit.large_refs.len());
            for lr in &commit.large_refs {
                let extent = ExtentId::new(cell.id, lr.sequence);
                let old_key = layout::extent(cell, old_epoch, lr.bucket_segment, extent);
                let meta = bucket::head(&**cold_bucket, &old_key).await?.ok_or_else(|| {
                    bucket_cell::Error::Lost(format!(
                        "fast WAL large extent {old_key} missing during recovery"
                    ))
                })?;
                if meta.size > MAX_RECOVERY_OWN_EXTENT_BYTES {
                    return Err(bucket_cell::Error::Corrupt(format!(
                        "fast WAL large extent {old_key} size {} exceeds recovery limit \
                         {MAX_RECOVERY_OWN_EXTENT_BYTES}",
                        meta.size
                    ))
                    .into());
                }
                let frame_bytes = {
                    let _slot = writer.budget().hold(Priority::Demand, meta.size).await;
                    let obj = bucket::get(&**cold_bucket, &old_key).await?.ok_or_else(|| {
                        bucket_cell::Error::Lost(format!(
                            "fast WAL large extent {old_key} missing during recovery"
                        ))
                    })?;
                    // Verify full frame structure, every part's stored CRC, and footer identity
                    // under the cell's keys before republishing (board 12295).
                    extent_frame::check(&obj.bytes).map_err(|err| {
                        bucket_cell::Error::Corrupt(format!(
                            "fast WAL large extent {old_key} failed frame check: {err}"
                        ))
                    })?;
                    let cell_keys = keys.of(cell.id);
                    let footer_bytes = extent_frame::footer(&obj.bytes, &cell_keys)
                        .map_err(bucket_cell::Error::from)?;
                    let verified = layout::decode_own_extent_footer(footer_bytes)?;
                    if verified != extent {
                        return Err(bucket_cell::Error::Corrupt(format!(
                            "fast WAL large extent {old_key} footer names {verified:?}, expected \
                             {extent:?}"
                        ))
                        .into());
                    }
                    obj.bytes
                };
                // Stream directly to `new_epoch`'s cold own-extent key under `writer.budget()`,
                // leaving 0 bytes buffered in `own_preframed` (`writer.upload` skips keys already
                // recorded in `preuploaded_own`).
                let new_key = layout::extent(cell, new_epoch, target_seg, extent);
                writer.put_large_extent(&new_key, frame_bytes).await?;
                own_preframed.push((lr.sequence, lr.logical_len, Vec::new()));
            }

            let versions = commit.versions;
            builder.commit(Commit {
                tag: commit.tag,
                extents: commit.inline_extents,
                own: Vec::new(),
                own_preframed,
                frees: commit.frees,
                root: commit.root,
                capacity: commit.capacity,
                next_sequence: commit.next_sequence,
            });
            builder_bytes += inline_bytes;

            if !versions.is_empty() {
                let objects = builder.close_with_versions(&mut chain, versions).frame(&encrypting);
                let seg_num = writer.add_segment(objects)?;
                writer.upload(seg_num).await?;
                last_uploaded_seg = Some(seg_num);
                builder = SegmentBuilder::default();
                builder_bytes = 0;
            }
        }

        if done {
            break;
        }
        seg += 1;
    }

    if !builder.is_empty() {
        let objects = builder.close(&mut chain).frame(&encrypting);
        let seg_num = writer.add_segment(objects)?;
        writer.upload(seg_num).await?;
        last_uploaded_seg = Some(seg_num);
    }

    // Atomically acknowledge all recovered segments AND switch `head.fast_wal` in ONE head CAS
    // (`activateAfterFlush`).
    if let Some(last_seg) = last_uploaded_seg {
        writer.activate_fast_wal_past(last_seg, target_fast_wal);
        writer.acknowledge().await?;
    } else {
        writer.activate_fast_wal(target_fast_wal).await?;
    }
    Ok(recovered_count)
}

/// Sweeps superseded fast WAL segments of `cell` in `fast_bucket`, overwriting each segment of an
/// epoch strictly older than `head.epoch` and not equal to `head.fast_wal.active.epoch` with an
/// authenticated 36-byte `CFTS` swept tombstone (`tombstoneOnSweep`, `sweepChecksActive`,
/// `sweepKeepsFence`, board 12299).
pub async fn sweep_superseded<F: Bucket>(
    fast_bucket: &Arc<F>,
    cell: CellId,
    head_epoch: u64,
    active_wal_epoch: Option<u64>,
    keys: &Ring,
) -> Result<usize, Failure> {
    let prefix = layout::wal_prefix(cell);
    let listed = bucket::list(&**fast_bucket, &prefix).await?;
    let mut swept = 0usize;
    for meta in listed {
        let Kind::Wal { cell: of, epoch, segment } = layout::kind(&meta.key) else { continue };
        if of != cell || epoch >= head_epoch || Some(epoch) == active_wal_epoch {
            continue;
        }
        // Already a 36-byte tombstone: skip to avoid redundant writes (`sweepKeepsFence`).
        if meta.size == TOMBSTONE_LEN as u64 {
            continue;
        }
        let tomb =
            wire::encode_tombstone(cell, epoch, segment, keys, Tombstone::Swept { head_epoch })?;
        if rules().wal_sweep_tombstones {
            bucket::overwrite(&**fast_bucket, &meta.key, Bytes::from(tomb.to_vec())).await?;
        } else if let Err(error) = fast_bucket.delete(&meta.key).await {
            // Left as it was, for the next sweep, as a failed overwrite is.
            log::warn!("cladefs: deleting superseded fast WAL segment {}: {error}", meta.key);
            continue;
        }
        swept += 1;
    }
    Ok(swept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bucket_cell::layout::Fsync;
    use bucket_cell::lifecycle;
    use bucket_cell::object_bucket::ObjectBucket;
    use bucket_cell::testing::SimBucket;
    use object_store::memory::InMemory;
    use std::future::Future;

    /// The host runtime's sleep, as the uploader's clock is in production.
    fn tokio_clock() -> Clock {
        Arc::new(|interval| tokio::time::sleep(interval).boxed())
    }

    fn memory_bucket() -> Arc<ObjectBucket> {
        Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""))
    }

    #[test]
    fn adaptive_batcher_zero_linger_when_serial_and_adapts_under_concurrency() {
        let mut batcher = AdaptiveBatcher::new();
        // Serial idle workload: 0 ms linger!
        assert_eq!(batcher.linger(1, 4096), Duration::ZERO);
        // Observe single-commit batch with 2 ms RTT: still serial -> 0 ms linger.
        batcher.observe(1, Duration::from_millis(2));
        assert_eq!(batcher.linger(1, 4096), Duration::ZERO);
        // Observe concurrent multi-commit batch with 4 ms RTT -> adaptive linger activates.
        batcher.observe(3, Duration::from_millis(4));
        let linger = batcher.linger(1, 4096);
        assert!(linger >= MIN_LINGER && linger <= MAX_LINGER, "linger={linger:?}");
        // Batch at size or commit cap flushes immediately even under concurrency.
        assert_eq!(batcher.linger(MAX_BATCH_COMMITS, 4096), Duration::ZERO);
        assert_eq!(batcher.linger(2, MAX_BATCH_BYTES), Duration::ZERO);
        // Returns to 0 ms linger once a batch has only 1 commit again.
        batcher.observe(1, Duration::from_millis(2));
        assert_eq!(batcher.linger(1, 4096), Duration::ZERO);
    }

    #[test]
    fn wire_decode_rejects_corrupted_headers_and_records() {
        let cell = layout::test_cell(5);
        let ring = bucket_cell::testing::ring([cell]);
        let (_, key_id) = ring.try_encrypting().unwrap();
        let header = WalHeader {
            key_generation: key_id.generation,
            cell,
            epoch: 2,
            segment: 0,
            salt: Salt::random(),
        };
        let header_bytes = wire::encode_header(&header);
        let aead = wire::derive_aead(&header, &ring).unwrap();
        let rec = wire::encode_record(&header_bytes, &aead, 0, &WalRecord::Rolled).unwrap();
        let mut valid = header_bytes.to_vec();
        valid.extend_from_slice(&rec);

        // Valid decode passes.
        let (_, _, decoded) = wire::decode_segment(&valid, cell, 2, 0, &ring).unwrap();
        assert_eq!(decoded, vec![(0, WalRecord::Rolled)]);

        // Wrong epoch or segment rejected.
        assert!(wire::decode_segment(&valid, cell, 3, 0, &ring).is_err());
        assert!(wire::decode_segment(&valid, cell, 2, 1, &ring).is_err());

        // Truncated header or body rejected.
        assert!(wire::decode_segment(&valid[..HEADER_LEN - 1], cell, 2, 0, &ring).is_err());
        assert!(wire::decode_segment(&valid[..valid.len() - 1], cell, 2, 0, &ring).is_err());

        // Flipped bit in header or tag rejected.
        let mut bad_hdr = valid.clone();
        bad_hdr[10] ^= 1;
        assert!(wire::decode_segment(&bad_hdr, cell, 2, 0, &ring).is_err());
        let mut bad_tag = valid.clone();
        *bad_tag.last_mut().unwrap() ^= 1;
        assert!(wire::decode_segment(&bad_tag, cell, 2, 0, &ring).is_err());

        // Trailing garbage rejected.
        let mut trailing = valid.clone();
        trailing.push(0xff);
        assert!(wire::decode_segment(&trailing, cell, 2, 0, &ring).is_err());
    }

    #[test]
    fn takeover_fences_recovers_large_extents_and_rejects_zombie_after_sweep() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let cold = memory_bucket();
            let fast = memory_bucket();
            let cell = layout::test_cell(9);
            lifecycle::create(&*cold, cell, 1).await.unwrap();

            // Writer 1 takes cell and activates fast WAL.
            let w1 = Arc::new(Writer::open(cold.clone(), cell, 10, 1).await.unwrap());
            let active1 = ActiveWal { epoch: w1.epoch(), bucket: "memory:///fast".into() };
            w1.activate_fast_wal(Some(FastWal {
                fsync: Fsync::FastWal,
                active: Some(active1.clone()),
            }))
            .await
            .unwrap();

            // Stage two commits with a small max_writer_appends=2 so rollover triggers, plus a
            // large own-extent in commit 2.
            let changed = Arc::new(Event::new());
            let sink = FastWalSink::new(
                "memory:///fast".into(),
                0,
                changed.clone(),
                Arc::new(|| {}),
                None,
            );
            let writer_task = tokio::spawn(
                FastWalWriter::new(
                    w1.clone(),
                    fast.clone(),
                    sink.clone(),
                    1,
                    DEFAULT_MAX_SEGMENT_BYTES,
                    tokio_clock(),
                )
                .run(),
            );

            let e1 = ExtentId::new(cell.id, w1.chain().next_sequence());
            let e2_seq = w1.chain().next_sequence() + 1;
            let e2 = ExtentId::new(cell.id, e2_seq);
            let large_data = vec![0x5a_u8; 8192];
            let large_framed =
                layout::own_extent(e2, &large_data, w1.keys().try_encrypting().unwrap());

            sink.push(
                vec![StagedCommit {
                    epoch: 1,
                    tag: layout::tag(w1.epoch(), 1),
                    inline_extents: vec![(e1, b"small-extent".to_vec())],
                    large_extents: Vec::new(),
                    frees: Vec::new(),
                    root: Some(b"root1".to_vec()),
                    capacity: Some(1 << 20),
                    next_sequence: e1.sequence + 1,
                    versions: Vec::new(),
                }],
                1,
            );
            while sink.durable() < 1 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            sink.push(
                vec![StagedCommit {
                    epoch: 2,
                    tag: layout::tag(w1.epoch(), 2),
                    inline_extents: Vec::new(),
                    large_extents: vec![StagedLargeExtent {
                        sequence: e2_seq,
                        logical_len: large_data.len() as u64,
                        bucket_segment: 0,
                        framed_bytes: large_framed,
                    }],
                    frees: Vec::new(),
                    root: Some(b"root2".to_vec()),
                    capacity: None,
                    next_sequence: e2_seq + 1,
                    versions: Vec::new(),
                }],
                2,
            );
            while sink.durable() < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            // Note: max_writer_appends = 1 rolled seg 0 -> seg 1 before commit 2!
            assert!(fast.head(&layout::wal(cell, w1.epoch(), 0)).await.is_ok());
            assert!(fast.head(&layout::wal(cell, w1.epoch(), 1)).await.is_ok());

            // Simulate writer 1 crash without cold upload: cold bucket head still has end == 0.
            sink.stop();
            let _ = writer_task.await;
            assert_eq!(w1.head().await.end, 0);

            // Verify collector protects epoch 1's large own-extent while head.fast_wal.active == ep 1!
            let w2 = Arc::new(Writer::open(cold.clone(), cell, 20, 2).await.unwrap());
            assert_eq!(w2.head().await.fast_wal.as_ref().unwrap().active, Some(active1.clone()));
            let gc_report = bucket_cell::collector::collect(&*cold, 99).await.unwrap();
            assert_eq!(gc_report.deleted, 0, "GC must protect active fast WAL large extents");

            // Writer 2 fences and recovers epoch 1 across seg 0 (Rolled) and seg 1 (open).
            let target = Some(FastWal {
                fsync: Fsync::FastWal,
                active: Some(ActiveWal { epoch: w2.epoch(), bucket: "memory:///fast".into() }),
            });
            let recovered =
                fence_and_recover(&w2, &cold, &fast, &active1, target.clone()).await.unwrap();
            assert_eq!(recovered, 2);
            assert_eq!(w2.state().root, Some(b"root2".to_vec()));
            assert!(w2.state().extents.contains_key(&e1));
            assert!(w2.state().extents.contains_key(&e2));
            assert_eq!(w2.head().await.fast_wal, target);

            // Verify large extent is readable from cold bucket at w2's key!
            let loc = w2.state().extents.get(&e2).unwrap().clone();
            let read_back = bucket_cell::fetch::read(&cold, w2.keys(), e2, &loc).await.unwrap();
            assert_eq!(read_back, large_data);

            // Sweep superseded epoch 1 -> overwrites with 36-byte authenticated `CFTS` tombstone.
            let swept = sweep_superseded(&fast, cell, w2.epoch(), Some(w2.epoch()), w2.keys())
                .await
                .unwrap();
            assert_eq!(swept, 2);
            let tomb = fast.get(&layout::wal(cell, w1.epoch(), 1)).await.unwrap();
            assert_eq!(tomb.bytes.len(), TOMBSTONE_LEN);
            assert_eq!(
                wire::decode_tombstone(&tomb.bytes, cell, w1.epoch(), 1, w2.keys()).unwrap(),
                Tombstone::Swept { head_epoch: w2.epoch() }
            );
            // Delayed zombie append at offset 0 or offset >= HEADER_LEN is rejected forever!
            assert_eq!(
                fast.append(&layout::wal(cell, w1.epoch(), 1), 0, Bytes::from_static(b"z")).await,
                Err(bucket::Error::Precondition)
            );
            assert_eq!(
                fast.append(
                    &layout::wal(cell, w1.epoch(), 1),
                    HEADER_LEN as u64,
                    Bytes::from_static(b"z"),
                )
                .await,
                Err(bucket::Error::Precondition)
            );
        });
    }

    #[test]
    fn gc_rule_mutation_deletes_wal_extents_when_protect_wal_epoch_is_off() {
        fn run(protect_wal_epoch: bool) -> (usize, bool) {
            bucket_cell::set_rules(bucket_cell::Rules {
                protect_wal_epoch,
                ..bucket_cell::Rules::ALL
            });
            let bucket = SimBucket::new();
            let cell = layout::test_cell(11);
            bucket.served(lifecycle::create(&*bucket, cell, 1)).0.unwrap();
            let w1 = bucket.served(Writer::open(bucket.clone(), cell, 10, 1)).0.unwrap();
            let active = ActiveWal { epoch: w1.epoch(), bucket: "sim://fast".into() };
            bucket
                .served(w1.activate_fast_wal(Some(FastWal {
                    fsync: Fsync::FastWal,
                    active: Some(active),
                })))
                .0
                .unwrap();
            let e = ExtentId::new(cell.id, w1.chain().next_sequence());
            let key = layout::extent(cell, w1.epoch(), 0, e);
            let framed = layout::own_extent(e, &[7u8; 4096], w1.keys().try_encrypting().unwrap());
            bucket.served(w1.put_large_extent(&key, framed)).0.unwrap();

            // Writer 2 takes head (epoch 2), preserving active = epoch 1.
            let _w2 = bucket.served(Writer::open(bucket.clone(), cell, 20, 2)).0.unwrap();

            let report = bucket.served(bucket_cell::collector::collect(&*bucket, 1)).0.unwrap();
            let kept = bucket.keys().contains(&key);
            bucket_cell::set_rules(bucket_cell::Rules::ALL);
            (report.deleted, kept)
        }

        // With protect_wal_epoch = true: GC keeps the large extent.
        assert_eq!(run(true), (0, true));
        // Mutation: with protect_wal_epoch = false, GC deletes it (mirrors GcDeletesWalExtents.cfg).
        assert_eq!(run(false), (1, false));
    }

    /// `tombstoneOnSweep` (rule `wal_sweep_tombstones`, `SweepDeletesFence.cfg`): a writer taken
    /// over tries its create of a segment again, the first having landed with its answer lost, and
    /// the retry lands only after the take fenced the segment, recovered it and swept it. Swept to
    /// a tombstone, the retry is refused; deleted, it recreates the segment without its fence, and
    /// the writer appends on, a commit told durable that the take never recovered. The simulation
    /// reaches this only by luck (board 14433): a retry in flight across a whole take and its sweep,
    /// its writer alive through them; no seed of 5,000 of its zombie profile did.
    #[test]
    fn sweeps_need_tombstones() {
        /// Whether writer 1's commit after its retried create lands is told durable.
        fn told_after_sweep(rules: bucket_cell::Rules) -> bool {
            bucket_cell::set_rules(rules);
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let told = rt.block_on(async {
                let (cold, fast) = (memory_bucket(), memory_bucket());
                let cell = layout::test_cell(12);
                lifecycle::create(&*cold, cell, 1).await.unwrap();
                let w1 = Arc::new(Writer::open(cold.clone(), cell, 10, 1).await.unwrap());
                let active1 = ActiveWal { epoch: w1.epoch(), bucket: "memory:///fast".into() };
                let fast_wal =
                    |active| Some(FastWal { fsync: Fsync::FastWal, active: Some(active) });
                w1.activate_fast_wal(fast_wal(active1.clone())).await.unwrap();
                let sink = FastWalSink::new(
                    "memory:///fast".into(),
                    0,
                    Arc::new(Event::new()),
                    Arc::new(|| {}),
                    None,
                );
                let mut writer = FastWalWriter::new(
                    w1.clone(),
                    fast.clone(),
                    sink,
                    DEFAULT_MAX_WRITER_APPENDS,
                    DEFAULT_MAX_SEGMENT_BYTES,
                    tokio_clock(),
                );
                writer.open = Some(OpenSegment::new(cell, w1.epoch(), 0, w1.keys()).unwrap());
                let sequence = w1.chain().next_sequence();
                let batch = |epoch: u64| {
                    WalRecord::Batch(vec![FastWalCommit {
                        epoch,
                        tag: layout::tag(w1.epoch(), epoch),
                        inline_extents: vec![(ExtentId::new(cell.id, sequence + epoch), vec![7])],
                        large_refs: Vec::new(),
                        frees: Vec::new(),
                        root: None,
                        capacity: Some(1 << 20),
                        next_sequence: sequence + epoch + 1,
                        versions: Vec::new(),
                    }])
                };
                // Writer 1's create of segment 0, which lands: its bytes, tried again later.
                assert!(writer.append_record(&batch(1), 1).await.unwrap());
                let key = layout::wal(cell, w1.epoch(), 0);
                let create = fast.get(&key).await.unwrap().bytes;

                // Writer 2 takes the cell, fences and recovers segment 0, and sweeps it.
                let w2 = Arc::new(Writer::open(cold.clone(), cell, 20, 2).await.unwrap());
                let active2 = ActiveWal { epoch: w2.epoch(), bucket: "memory:///fast".into() };
                let recovered =
                    fence_and_recover(&w2, &cold, &fast, &active1, fast_wal(active2)).await;
                assert_eq!(recovered.unwrap(), 1);
                sweep_superseded(&fast, cell, w2.epoch(), Some(w2.epoch()), w2.keys())
                    .await
                    .unwrap();

                // Writer 1's retry of its create lands now, or is refused; then its next commit.
                match fast.append(&key, 0, Bytes::from(create)).await {
                    Ok(_) => writer.append_record(&batch(2), 2).await.unwrap(),
                    Err(error) => {
                        assert_eq!(error, bucket::Error::Precondition);
                        false
                    }
                }
            });
            bucket_cell::set_rules(bucket_cell::Rules::ALL);
            told
        }

        assert!(!told_after_sweep(bucket_cell::Rules::ALL));
        let deleting =
            bucket_cell::Rules { wal_sweep_tombstones: false, ..bucket_cell::Rules::ALL };
        assert!(told_after_sweep(deleting));
    }

    /// A bucket that refuses its first `refusals` fence records (`CFWF`) as S3 refuses a write
    /// conflicting with one in progress (409, `Precondition`), the object unchanged.
    struct Conflicting {
        inner: Arc<ObjectBucket>,
        refusals: AtomicU64,
    }

    impl Bucket for Conflicting {
        fn get(
            &self,
            key: &str,
        ) -> impl Future<Output = Result<bucket::Object, bucket::Error>> + Send {
            self.inner.get(key)
        }

        fn get_range(
            &self,
            key: &str,
            range: bucket::ByteRange,
        ) -> impl Future<Output = Result<Vec<u8>, bucket::Error>> + Send {
            self.inner.get_range(key, range)
        }

        fn head(
            &self,
            key: &str,
        ) -> impl Future<Output = Result<bucket::Meta, bucket::Error>> + Send {
            self.inner.head(key)
        }

        fn list(
            &self,
            prefix: &str,
        ) -> impl Future<Output = Result<Vec<bucket::Meta>, bucket::Error>> + Send {
            self.inner.list(prefix)
        }

        fn list_delimited(
            &self,
            prefix: &str,
            page: bucket::Page<'_>,
        ) -> impl Future<Output = Result<bucket::Listing, bucket::Error>> + Send {
            self.inner.list_delimited(prefix, page)
        }

        fn put(
            &self,
            key: &str,
            bytes: Bytes,
            mode: bucket::PutMode,
        ) -> impl Future<Output = Result<bucket::Version, bucket::Error>> + Send {
            self.inner.put(key, bytes, mode)
        }

        async fn append(&self, key: &str, offset: u64, bytes: Bytes) -> Result<u64, bucket::Error> {
            let refused = bytes.starts_with(&FENCE_MAGIC)
                && self
                    .refusals
                    .try_update(Ordering::AcqRel, Ordering::Acquire, |left| left.checked_sub(1))
                    .is_ok();
            match refused {
                true => Err(bucket::Error::Precondition),
                false => self.inner.append(key, offset, bytes).await,
            }
        }

        fn delete(&self, key: &str) -> impl Future<Output = Result<(), bucket::Error>> + Send {
            self.inner.delete(key)
        }

        fn now(&self) -> u64 {
            self.inner.now()
        }
    }

    /// A take's fence refused at the segment's end, nobody appending, is a write in progress
    /// that ends (S3's 409, which `ObjectBucket` answers `Precondition`): the take backs off and
    /// fences once it ends; refused `FENCE_REFUSALS` times, the segment is at its appends' limit,
    /// and the take fails rather than try for ever (verifying review of board 14433).
    #[test]
    fn takes_outlast_conflicting_writes_but_not_full_segments() {
        fn take(refusals: u64) -> Result<usize, Failure> {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                let (cold, inner) = (memory_bucket(), memory_bucket());
                let cell = layout::test_cell(13);
                lifecycle::create(&*cold, cell, 1).await.unwrap();
                let w1 = Arc::new(Writer::open(cold.clone(), cell, 10, 1).await.unwrap());
                let active1 = ActiveWal { epoch: w1.epoch(), bucket: "memory:///fast".into() };
                let fast_wal =
                    |active| Some(FastWal { fsync: Fsync::FastWal, active: Some(active) });
                w1.activate_fast_wal(fast_wal(active1.clone())).await.unwrap();
                let sink = FastWalSink::new(
                    "memory:///fast".into(),
                    0,
                    Arc::new(Event::new()),
                    Arc::new(|| {}),
                    None,
                );
                let mut writer = FastWalWriter::new(
                    w1.clone(),
                    inner.clone(),
                    sink,
                    DEFAULT_MAX_WRITER_APPENDS,
                    DEFAULT_MAX_SEGMENT_BYTES,
                    tokio_clock(),
                );
                writer.open = Some(OpenSegment::new(cell, w1.epoch(), 0, w1.keys()).unwrap());
                let sequence = w1.chain().next_sequence();
                let batch = WalRecord::Batch(vec![FastWalCommit {
                    epoch: 1,
                    tag: layout::tag(w1.epoch(), 1),
                    inline_extents: vec![(ExtentId::new(cell.id, sequence), vec![7])],
                    large_refs: Vec::new(),
                    frees: Vec::new(),
                    root: None,
                    capacity: Some(1 << 20),
                    next_sequence: sequence + 1,
                    versions: Vec::new(),
                }]);
                assert!(writer.append_record(&batch, 1).await.unwrap());

                let fast = Arc::new(Conflicting { inner, refusals: AtomicU64::new(refusals) });
                let w2 = Arc::new(Writer::open(cold.clone(), cell, 20, 2).await.unwrap());
                let active2 = ActiveWal { epoch: w2.epoch(), bucket: "memory:///fast".into() };
                fence_and_recover(&w2, &cold, &fast, &active1, fast_wal(active2)).await
            })
        }

        assert_eq!(take(2).unwrap(), 1);
        let full = take(u64::from(FENCE_REFUSALS)).unwrap_err().to_string();
        assert!(full.contains("it can be fenced never"), "{full}");
    }

    struct MapOpener(std::collections::HashMap<String, Arc<AnyBucket>>);

    impl FastWalOpener for MapOpener {
        fn open(&self, url: &str) -> BoxFuture<'static, Result<Arc<AnyBucket>, String>> {
            let found =
                self.0.get(url).cloned().ok_or_else(|| format!("unknown fast WAL bucket {url}"));
            async move { found }.boxed()
        }
    }

    /// doc-29: full `upload::open` lifecycle with Fast WAL: inline + large (>= 256 KiB) writes,
    /// complete loss of local disk (`MemDir` wiped without cold sync), cross-AZ failover recovery,
    /// zero double upload of pre-uploaded large extents, and clean disable flush.
    #[tokio::test(flavor = "multi_thread")]
    async fn upload_open_fast_wal_crash_wipe_local_disk_recovers_inline_and_large_and_cross_az() {
        use crate::{Description, LocalCell, MemDir, upload};
        use bucket_cell::fetch::{Budget, Limits};

        let host = upload::Runtime {
            handle: tokio::runtime::Handle::current(),
            uploads: Budget::new(Limits::UNBOUNDED),
            keys: bucket_cell::keys::testing::host(),
        };
        let cold = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "cold").unrouted());
        let fast_az1 = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "az1"));
        let fast_az2 = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "az2"));
        let mut map = std::collections::HashMap::new();
        let any = |bucket: &Arc<ObjectBucket>| Arc::new(AnyBucket::new(bucket.clone()));
        map.insert("memory:///fast-az1".to_owned(), any(&fast_az1));
        map.insert("memory:///fast-az2".to_owned(), any(&fast_az2));
        let opener: Arc<dyn FastWalOpener> = Arc::new(MapOpener(map));

        let cell_id = layout::test_cell(42);
        lifecycle::create(&*cold, cell_id, 1).await.unwrap();
        let desc = Description { block_size: 4096, capacity: 1 << 30, cell: cell_id };
        let kind = bucket_cell::cell::Kind::Fxfs;

        // Writer 1 in AZ 1: interval = MAX so cold bucket receives zero segment uploads!
        let dir1 = MemDir::new();
        LocalCell::make(&dir1, desc, Some("memory:///cold"), Some(kind)).unwrap();
        let opts1 = upload::Options {
            interval: Duration::MAX,
            upload_closed: false,
            token: 1,
            fast_wal: Some(Some(Fsync::FastWal)),
            fast_wal_config: Some(FastWalConfig::new(
                "memory:///fast-az1".to_owned(),
                any(&fast_az1),
                opener.clone(),
            )),
            fast_wal_large_extent: 256 << 10,
            ..upload::Options::default()
        };
        let (cell1, up1) = upload::open(
            LocalCell::recover_unmade(dir1).unwrap(),
            crate::Options::default(),
            cold.clone(),
            (10, opts1),
            &host,
        )
        .await
        .unwrap();
        cell1.made().unwrap();
        assert_eq!(up1.fast_wal_fsync(), Some(Fsync::FastWal));
        assert_eq!(
            up1.status().unwrap().fast_wal.as_ref().map(|fw| (fw.bucket.as_str(), fw.degraded)),
            Some(("memory:///fast-az1", false))
        );

        let e_small = ExtentId::new(cell_id.id, up1.next_sequence());
        let e_large = ExtentId::new(cell_id.id, up1.next_sequence() + 1);
        let small_data = vec![0x33u8; 8192];
        let large_data = vec![0x77u8; 300 << 10];
        cell1.write(e_small, 0, &small_data).await.unwrap();
        cell1.write(e_large, 0, &large_data).await.unwrap();
        cell1.commit_ordered(Some(b"fast-wal-root-1")).await.unwrap();
        cell1.flush().await.unwrap();
        let commit1 = cell1.last_commit().unwrap();
        up1.fast_wal(commit1).await.unwrap();

        // Cold bucket head has NOT acknowledged any segment yet, yet large extent is already there!
        assert_eq!(up1.last_uploaded(), 0);
        let head1 = bucket_cell::head::get(&*cold, cell_id).await.unwrap().unwrap();
        assert_eq!(head1.end, 0);
        assert_eq!(
            head1
                .fast_wal
                .as_ref()
                .and_then(|fw| fw.active.as_ref())
                .map(|a| (a.epoch, a.bucket.as_str())),
            Some((1, "memory:///fast-az1"))
        );

        // Simulate abrupt node loss: cut uploader short (no cold upload) and discard dir1 completely!
        up1.cut_short()();
        drop(cell1);
        up1.close().await;

        // Writer 2 in AZ 2 on a brand-new empty MemDir (zero local state!).
        let dir2 = MemDir::new();
        LocalCell::make(&dir2, desc, Some("memory:///cold"), Some(kind)).unwrap();
        let opts2 = upload::Options {
            interval: Duration::MAX,
            upload_closed: false,
            token: 2,
            fast_wal: None, // inherits FastWal from head
            fast_wal_config: Some(FastWalConfig::new(
                "memory:///fast-az2".to_owned(),
                any(&fast_az2),
                opener.clone(),
            )),
            fast_wal_large_extent: 256 << 10,
            ..upload::Options::default()
        };
        let (cell2, up2) = upload::open(
            LocalCell::recover_unmade(dir2).unwrap(),
            crate::Options::default(),
            cold.clone(),
            (20, opts2),
            &host,
        )
        .await
        .unwrap();
        cell2.made().unwrap();
        assert_eq!(cell2.root().unwrap(), b"fast-wal-root-1");
        let head2_for_rec = bucket_cell::head::get(&*cold, cell_id).await.unwrap();
        let rec2 = bucket_cell::recovery::recover_under_from(
            &*cold,
            cell_id,
            None,
            head2_for_rec,
            cell2.keys(),
        )
        .await
        .unwrap();
        assert_eq!(
            bucket_cell::fetch::read(
                &cold,
                cell2.keys(),
                e_small,
                rec2.state.extents.get(&e_small).unwrap()
            )
            .await
            .unwrap(),
            small_data
        );
        assert_eq!(
            bucket_cell::fetch::read(
                &cold,
                cell2.keys(),
                e_large,
                rec2.state.extents.get(&e_large).unwrap()
            )
            .await
            .unwrap(),
            large_data
        );

        let head2 = bucket_cell::head::get(&*cold, cell_id).await.unwrap().unwrap();
        assert_eq!(
            head2
                .fast_wal
                .as_ref()
                .and_then(|fw| fw.active.as_ref())
                .map(|a| (a.epoch, a.bucket.as_str())),
            Some((2, "memory:///fast-az2"))
        );

        // Zero double upload: commit a new 300 KiB large extent, wait for fast WAL, then wait for
        // cold upload, and confirm the cold bucket saw ONLY ONE create for that extent!
        let creates_before = cold.stats().requests(bucket_cell::object_bucket::Request::Create);
        let e_large2 = ExtentId::new(cell_id.id, up2.next_sequence());
        let large_data2 = vec![0x99u8; 300 << 10];
        cell2.write(e_large2, 0, &large_data2).await.unwrap();
        cell2.commit_ordered(Some(b"fast-wal-root-2")).await.unwrap();
        cell2.flush().await.unwrap();
        let commit2 = cell2.last_commit().unwrap();
        up2.fast_wal(commit2).await.unwrap();
        let creates_after_fast_wal =
            cold.stats().requests(bucket_cell::object_bucket::Request::Create);
        assert_eq!(
            creates_after_fast_wal - creates_before,
            1,
            "large extent pre-uploaded directly to cold bucket once"
        );
        up2.uploaded(commit2).await.unwrap();
        let creates_after_cold = cold.stats().requests(bucket_cell::object_bucket::Request::Create);
        // Only the cold segment object itself should be created (1), NOT the own-extent again!
        assert_eq!(
            creates_after_cold - creates_after_fast_wal,
            1,
            "cold upload must skip pre-uploaded own-extent (zero double upload)"
        );

        // Commit one more unuploaded small extent in AZ 2, crash writer 2, then disable Fast WAL
        // on Writer 3 (`fast_wal: Some(None)`) and confirm it flushes and clears `head.fast_wal`.
        let e_small2 = ExtentId::new(cell_id.id, up2.next_sequence() + 1);
        let small_data2 = vec![0x55u8; 4096];
        cell2.write(e_small2, 0, &small_data2).await.unwrap();
        cell2.commit_ordered(Some(b"fast-wal-root-3")).await.unwrap();
        cell2.flush().await.unwrap();
        up2.fast_wal(cell2.last_commit().unwrap()).await.unwrap();
        up2.cut_short()();
        drop(cell2);
        up2.close().await;

        let dir3 = MemDir::new();
        LocalCell::make(&dir3, desc, Some("memory:///cold"), Some(kind)).unwrap();
        let opts3 = upload::Options {
            interval: Duration::MAX,
            upload_closed: false,
            token: 3,
            fast_wal: Some(None),
            fast_wal_config: Some(FastWalConfig::new(
                "memory:///fast-az2".to_owned(),
                any(&fast_az2),
                opener,
            )),
            ..upload::Options::default()
        };
        let (cell3, up3) = upload::open(
            LocalCell::recover_unmade(dir3).unwrap(),
            crate::Options::default(),
            cold.clone(),
            (30, opts3),
            &host,
        )
        .await
        .unwrap();
        cell3.made().unwrap();
        assert_eq!(cell3.root().unwrap(), b"fast-wal-root-3");
        let head3_for_rec = bucket_cell::head::get(&*cold, cell_id).await.unwrap();
        let rec3 = bucket_cell::recovery::recover_under_from(
            &*cold,
            cell_id,
            None,
            head3_for_rec,
            cell3.keys(),
        )
        .await
        .unwrap();
        assert_eq!(
            bucket_cell::fetch::read(
                &cold,
                cell3.keys(),
                e_small2,
                rec3.state.extents.get(&e_small2).unwrap()
            )
            .await
            .unwrap(),
            small_data2
        );
        assert_eq!(up3.fast_wal_fsync(), None);
        let head3 = bucket_cell::head::get(&*cold, cell_id).await.unwrap().unwrap();
        assert_eq!(head3.fast_wal, None);
        drop(cell3);
        up3.close().await;
    }

    #[test]
    fn cfwf_fence_rejects_tampered_offset_and_epoch_and_preserves_next_sequence() {
        let cell = layout::test_cell(15);
        let ring = bucket_cell::testing::ring([cell]);
        let (_, key_id) = ring.try_encrypting().unwrap();
        let header = WalHeader {
            key_generation: key_id.generation,
            cell,
            epoch: 2,
            segment: 0,
            salt: Salt::random(),
        };
        let header_bytes = wire::encode_header(&header);
        let aead = wire::derive_aead(&header, &ring).unwrap();
        let batch = WalRecord::Batch(vec![FastWalCommit {
            epoch: 1,
            tag: layout::tag(2, 1),
            inline_extents: Vec::new(),
            large_refs: Vec::new(),
            frees: Vec::new(),
            root: Some(b"root-with-high-seq".to_vec()),
            capacity: None,
            next_sequence: 42,
            versions: Vec::new(),
        }]);
        let rec = wire::encode_record(&header_bytes, &aead, 0, &batch).unwrap();
        let fence_offset = (HEADER_LEN + rec.len()) as u64;
        let fence = wire::encode_fence(&header, &header_bytes, &ring, fence_offset, 3).unwrap();

        let mut valid = header_bytes.to_vec();
        valid.extend_from_slice(&rec);
        valid.extend_from_slice(&fence);

        let (_, _, decoded) = wire::decode_segment(&valid, cell, 2, 0, &ring).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0], (0, batch));
        assert_eq!(decoded[1], (u64::MAX, WalRecord::Fence { by_epoch: 3 }));

        // `encode_record` refuses `WalRecord::Fence` inside writer `CFWR` frames.
        assert!(
            wire::encode_record(&header_bytes, &aead, 0, &WalRecord::Fence { by_epoch: 3 })
                .is_err()
        );

        // A `CFWF` fence replayed at a different byte offset fails AEAD authentication.
        let mut shifted = header_bytes.to_vec();
        shifted.extend_from_slice(&fence);
        assert!(wire::decode_segment(&shifted, cell, 2, 0, &ring).is_err());

        // A `CFWF` fence with `by_epoch <= header.epoch` is rejected.
        let stale_fence =
            wire::encode_fence(&header, &header_bytes, &ring, HEADER_LEN as u64, 2).unwrap();
        let mut stale = header_bytes.to_vec();
        stale.extend_from_slice(&stale_fence);
        assert!(wire::decode_segment(&stale, cell, 2, 0, &ring).is_err());

        // Verify `fence_and_recover` preserves `next_sequence` even on a commit with zero extents.
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let cold = memory_bucket();
            let fast = memory_bucket();
            lifecycle::create(&*cold, cell, 1).await.unwrap();
            let w1 = Arc::new(Writer::open(cold.clone(), cell, 10, 1).await.unwrap());
            let active1 = ActiveWal { epoch: w1.epoch(), bucket: "memory:///fast".into() };
            w1.activate_fast_wal(Some(FastWal {
                fsync: Fsync::FastWal,
                active: Some(active1.clone()),
            }))
            .await
            .unwrap();
            let seg_hdr = WalHeader {
                key_generation: w1.keys().try_encrypting().unwrap().1.generation,
                cell,
                epoch: w1.epoch(),
                segment: 0,
                salt: Salt::random(),
            };
            let hdr_bytes = wire::encode_header(&seg_hdr);
            let seg_aead = wire::derive_aead(&seg_hdr, w1.keys()).unwrap();
            let r0 = wire::encode_record(
                &hdr_bytes,
                &seg_aead,
                0,
                &WalRecord::Batch(vec![FastWalCommit {
                    epoch: 1,
                    tag: layout::tag(w1.epoch(), 1),
                    inline_extents: Vec::new(),
                    large_refs: Vec::new(),
                    frees: Vec::new(),
                    root: Some(b"seq-jump".to_vec()),
                    capacity: None,
                    next_sequence: 99,
                    versions: Vec::new(),
                }]),
            )
            .unwrap();
            let mut obj = hdr_bytes.to_vec();
            obj.extend_from_slice(&r0);
            fast.append(&layout::wal(cell, w1.epoch(), 0), 0, Bytes::from(obj)).await.unwrap();

            let w2 = Arc::new(Writer::open(cold.clone(), cell, 20, 2).await.unwrap());
            let target = Some(FastWal {
                fsync: Fsync::FastWal,
                active: Some(ActiveWal { epoch: w2.epoch(), bucket: "memory:///fast".into() }),
            });
            let recovered = fence_and_recover(&w2, &cold, &fast, &active1, target).await.unwrap();
            assert_eq!(recovered, 1);
            assert!(
                w2.chain().next_sequence() >= 99,
                "recovered writer next_sequence {} must preserve commit next_sequence 99",
                w2.chain().next_sequence()
            );
        });
    }

    #[test]
    fn delayed_append_reconciles_offset_without_falsely_acking_next_batch() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let cold = memory_bucket();
            let fast = memory_bucket();
            let cell = layout::test_cell(16);
            lifecycle::create(&*cold, cell, 1).await.unwrap();
            let w1 = Arc::new(Writer::open(cold.clone(), cell, 10, 1).await.unwrap());
            let changed = Arc::new(Event::new());
            let sink = FastWalSink::new("memory:///fast".into(), 0, changed, Arc::new(|| {}), None);
            let mut writer = FastWalWriter::new(
                w1.clone(),
                fast.clone(),
                sink,
                DEFAULT_MAX_WRITER_APPENDS,
                DEFAULT_MAX_SEGMENT_BYTES,
                tokio_clock(),
            );
            let seg = OpenSegment::new(cell, w1.epoch(), 0, w1.keys()).unwrap();
            let key = layout::wal(cell, w1.epoch(), 0);

            // Simulate batch A (seq=0) timing out locally while landing late on the fast bucket:
            // the writer advanced next_record_seq to 1 before sealing A, then dropped A after cold
            // head ack so open.offset is still 0, while batch A actually landed on the server!
            let rec_a = WalRecord::Batch(vec![FastWalCommit {
                epoch: 1,
                tag: layout::tag(w1.epoch(), 1),
                inline_extents: Vec::new(),
                large_refs: Vec::new(),
                frees: Vec::new(),
                root: Some(b"batch-A".to_vec()),
                capacity: None,
                next_sequence: 1,
                versions: Vec::new(),
            }]);
            let enc_a = wire::encode_record(&seg.header_bytes, &seg.aead, 0, &rec_a).unwrap();
            let mut raw_a = seg.header_bytes.to_vec();
            raw_a.extend_from_slice(&enc_a);
            fast.append(&key, 0, Bytes::from(raw_a)).await.unwrap();

            let mut stale_seg = seg;
            stale_seg.next_record_seq = 1; // A already consumed seq=0
            stale_seg.offset = 0; // writer never saw A's reply
            writer.open = Some(stale_seg);
            writer.next_segment = 1;

            // Now the writer tries to append batch B at stale offset 0. It MUST NOT treat batch A
            // as an acknowledgement of batch B; it must reconcile offset, mint a fresh seq, and
            // append batch B after batch A.
            let rec_b = WalRecord::Batch(vec![FastWalCommit {
                epoch: 2,
                tag: layout::tag(w1.epoch(), 2),
                inline_extents: Vec::new(),
                large_refs: Vec::new(),
                frees: Vec::new(),
                root: Some(b"batch-B".to_vec()),
                capacity: None,
                next_sequence: 2,
                versions: Vec::new(),
            }]);
            let landed = writer.append_record(&rec_b, 2).await.unwrap();
            assert!(landed);

            let obj = fast.get(&key).await.unwrap();
            let (_, _, recs) =
                wire::decode_segment(&obj.bytes, cell, w1.epoch(), 0, w1.keys()).unwrap();
            assert_eq!(recs.len(), 2, "both delayed batch A and new batch B must be in WAL");
            assert_eq!(recs[0].1, rec_a);
            assert_eq!(recs[1].1, rec_b);
            assert!(recs[1].0 > recs[0].0, "batch B must have strictly higher seq than batch A");
        });
    }

    /// Board 12295: damaging a data part while keeping the footer intact must be rejected by both
    /// Fast WAL pre-upload (`put_large_extent`) and takeover recovery (`fence_and_recover`).
    #[test]
    fn corrupt_large_extent_data_part_rejected_on_preupload_and_takeover() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let cold = memory_bucket();
            let fast = memory_bucket();
            let cell = layout::test_cell(17);
            lifecycle::create(&*cold, cell, 1).await.unwrap();
            let w1 = Arc::new(Writer::open(cold.clone(), cell, 10, 1).await.unwrap());
            let active1 = ActiveWal { epoch: w1.epoch(), bucket: "memory:///fast".into() };
            w1.activate_fast_wal(Some(FastWal {
                fsync: Fsync::FastWal,
                active: Some(active1.clone()),
            }))
            .await
            .unwrap();

            let seq = w1.chain().next_sequence();
            let extent = ExtentId::new(cell.id, seq);
            let data = vec![0x42u8; 8192];
            let mut damaged =
                layout::own_extent(extent, &data, w1.keys().try_encrypting().unwrap());
            // Flip a byte inside the first data part while keeping the authenticated footer intact.
            damaged[0] ^= 0xff;
            assert!(
                extent_frame::footer(&damaged, &w1.keys().of(cell.id)).is_ok(),
                "footer itself must remain intact for this test"
            );

            // 1. Pre-upload rejects the damaged frame before creating it in the cold bucket.
            let old_key = layout::extent(cell, w1.epoch(), 0, extent);
            let err = w1.put_large_extent(&old_key, damaged.clone()).await.unwrap_err();
            assert!(matches!(err, bucket_cell::Error::Corrupt(_)), "expected Corrupt, got {err:?}");
            assert!(bucket::get(&*cold, &old_key).await.unwrap().is_none());

            // 2. If the damaged frame is somehow already stored in the cold bucket and referenced
            //    by an active fast WAL segment, takeover MUST refuse it rather than republishing.
            bucket::overwrite(&*cold, &old_key, Bytes::from(damaged)).await.unwrap();
            let seg_hdr = WalHeader {
                key_generation: w1.keys().try_encrypting().unwrap().1.generation,
                cell,
                epoch: w1.epoch(),
                segment: 0,
                salt: Salt::random(),
            };
            let hdr_bytes = wire::encode_header(&seg_hdr);
            let seg_aead = wire::derive_aead(&seg_hdr, w1.keys()).unwrap();
            let rec = wire::encode_record(
                &hdr_bytes,
                &seg_aead,
                0,
                &WalRecord::Batch(vec![FastWalCommit {
                    epoch: 1,
                    tag: layout::tag(w1.epoch(), 1),
                    inline_extents: Vec::new(),
                    large_refs: vec![LargeRef {
                        sequence: seq,
                        logical_len: data.len() as u64,
                        bucket_segment: 0,
                    }],
                    frees: Vec::new(),
                    root: Some(b"damaged-large".to_vec()),
                    capacity: None,
                    next_sequence: seq + 1,
                    versions: Vec::new(),
                }]),
            )
            .unwrap();
            let mut seg_bytes = hdr_bytes.to_vec();
            seg_bytes.extend_from_slice(&rec);
            fast.append(&layout::wal(cell, w1.epoch(), 0), 0, Bytes::from(seg_bytes))
                .await
                .unwrap();

            let w2 = Arc::new(Writer::open(cold.clone(), cell, 20, 2).await.unwrap());
            let target = Some(FastWal {
                fsync: Fsync::FastWal,
                active: Some(ActiveWal { epoch: w2.epoch(), bucket: "memory:///fast".into() }),
            });
            let rec_err = fence_and_recover(&w2, &cold, &fast, &active1, target).await.unwrap_err();
            assert!(
                matches!(rec_err, Failure::Bucket(bucket_cell::Error::Corrupt(_))),
                "takeover must reject damaged own-extent part, got {rec_err:?}"
            );
            assert_eq!(w2.head().await.fast_wal.as_ref().unwrap().active, Some(active1));
        });
    }

    /// Board 12299: replacing an active WAL segment with a forged, swept (`CFTS`), or premature
    /// rolled (`CFTR`) tombstone must cause takeover to fail hard and preserve `head.fast_wal.active`.
    #[test]
    fn forged_or_premature_tombstone_refused_preserving_active_head() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let cold = memory_bucket();
            let fast = memory_bucket();
            let cell = layout::test_cell(18);
            lifecycle::create(&*cold, cell, 1).await.unwrap();
            let w1 = Arc::new(Writer::open(cold.clone(), cell, 10, 1).await.unwrap());
            let active1 = ActiveWal { epoch: w1.epoch(), bucket: "memory:///fast".into() };
            w1.activate_fast_wal(Some(FastWal {
                fsync: Fsync::FastWal,
                active: Some(active1.clone()),
            }))
            .await
            .unwrap();

            let w2 = Arc::new(Writer::open(cold.clone(), cell, 20, 2).await.unwrap());
            let target = Some(FastWal {
                fsync: Fsync::FastWal,
                active: Some(ActiveWal { epoch: w2.epoch(), bucket: "memory:///fast".into() }),
            });
            let wal_key = layout::wal(cell, w1.epoch(), 0);

            // 1. Forged 36-byte tombstone (bad MAC).
            let forged = vec![0xaa; TOMBSTONE_LEN];
            bucket::overwrite(&*fast, &wal_key, Bytes::from(forged)).await.unwrap();
            let err =
                fence_and_recover(&w2, &cold, &fast, &active1, target.clone()).await.unwrap_err();
            assert!(matches!(err, Failure::Bucket(bucket_cell::Error::Corrupt(_))));
            assert_eq!(w2.head().await.fast_wal.as_ref().unwrap().active, Some(active1.clone()));

            // 2. Valid `CFTS` swept tombstone placed on an active WAL segment.
            let swept = wire::encode_tombstone(
                cell,
                w1.epoch(),
                0,
                w2.keys(),
                Tombstone::Swept { head_epoch: w2.epoch() },
            )
            .unwrap();
            bucket::overwrite(&*fast, &wal_key, Bytes::from(swept.to_vec())).await.unwrap();
            let err =
                fence_and_recover(&w2, &cold, &fast, &active1, target.clone()).await.unwrap_err();
            assert!(matches!(err, Failure::Bucket(bucket_cell::Error::Corrupt(_))));
            assert_eq!(w2.head().await.fast_wal.as_ref().unwrap().active, Some(active1.clone()));

            // 3. Valid `CFTR` rolled tombstone claiming commit epoch 5 when cold head is at 0.
            let premature_rolled = wire::encode_tombstone(
                cell,
                w1.epoch(),
                0,
                w2.keys(),
                Tombstone::Rolled { max_commit_epoch: 5 },
            )
            .unwrap();
            bucket::overwrite(&*fast, &wal_key, Bytes::from(premature_rolled.to_vec()))
                .await
                .unwrap();
            let err = fence_and_recover(&w2, &cold, &fast, &active1, target).await.unwrap_err();
            assert!(matches!(err, Failure::Bucket(bucket_cell::Error::Corrupt(_))));
            assert_eq!(w2.head().await.fast_wal.as_ref().unwrap().active, Some(active1));
        });
    }

    /// Board 12331: staging commits larger than the writer's upload `Budget` (with `requests: 1`)
    /// must never deadlock `put_large_extent` or cold uploads, while still reporting staged bytes
    /// to `HotBudget` and releasing them once flushed.
    #[test]
    fn small_upload_budget_never_deadlocks_fast_wal_staging() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let cold = memory_bucket();
            let fast = memory_bucket();
            let cell = layout::test_cell(19);
            lifecycle::create(&*cold, cell, 1).await.unwrap();
            let tiny_budget = bucket_cell::fetch::Budget::new(bucket_cell::fetch::Limits {
                requests: 1,
                bytes: 1,
            });
            let w1 = Arc::new(
                Writer::open(cold.clone(), cell, 10, 1)
                    .await
                    .unwrap()
                    .uploading_within(tiny_budget),
            );
            let hot_budget = HotBudget::new();
            let changed = Arc::new(Event::new());
            let sink = FastWalSink::new(
                "memory:///fast".into(),
                0,
                changed,
                Arc::new(|| {}),
                Some(hot_budget.clone()),
            );
            let seq = w1.chain().next_sequence();
            let e = ExtentId::new(cell.id, seq);
            let framed =
                layout::own_extent(e, &[0x21u8; 8192], w1.keys().try_encrypting().unwrap());
            let framed_len = framed.len() as u64;
            // Push two commits (exceeding requests=1 and bytes=1) BEFORE spawning the writer.
            sink.push(
                vec![
                    StagedCommit {
                        epoch: 1,
                        tag: layout::tag(w1.epoch(), 1),
                        inline_extents: Vec::new(),
                        large_extents: vec![StagedLargeExtent {
                            sequence: seq,
                            logical_len: 8192,
                            bucket_segment: 0,
                            framed_bytes: framed,
                        }],
                        frees: Vec::new(),
                        root: Some(b"r1".to_vec()),
                        capacity: None,
                        next_sequence: seq + 1,
                        versions: Vec::new(),
                    },
                    StagedCommit {
                        epoch: 2,
                        tag: layout::tag(w1.epoch(), 2),
                        inline_extents: vec![(ExtentId::new(cell.id, seq + 1), vec![0x33u8; 4096])],
                        large_extents: Vec::new(),
                        frees: Vec::new(),
                        root: Some(b"r2".to_vec()),
                        capacity: None,
                        next_sequence: seq + 2,
                        versions: Vec::new(),
                    },
                ],
                2,
            );
            assert!(
                hot_budget.bytes() >= framed_len + 4096,
                "staged bytes ({}) must be counted in HotBudget",
                hot_budget.bytes()
            );

            let writer_task = tokio::spawn(
                FastWalWriter::new(
                    w1.clone(),
                    fast.clone(),
                    sink.clone(),
                    DEFAULT_MAX_WRITER_APPENDS,
                    DEFAULT_MAX_SEGMENT_BYTES,
                    tokio_clock(),
                )
                .run(),
            );
            tokio::time::timeout(Duration::from_secs(5), async {
                while sink.durable() < 2 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("fast WAL staging must not deadlock under a tiny upload Budget");
            sink.stop();
            let _ = writer_task.await;
            assert_eq!(
                hot_budget.bytes(),
                0,
                "HotBudget must release all staged bytes after flush"
            );
        });
    }

    /// Board 12297: frozen golden corpus for the `fast wal` format family (`formats/fast-wal/`).
    #[test]
    fn every_frozen_version_is_read() {
        formats::golden::check::<FastWalFormat>(env!("CARGO_MANIFEST_DIR"));
    }

    struct FastWalFormat;

    const GOLDEN_CELL: CellId = CellId { tenant: 0x2f, id: (3 << 48) | 7 };
    const GOLDEN_EPOCH: u64 = 2;
    const GOLDEN_SEG: u64 = 0;
    const GOLDEN_SALT: Salt = Salt([0x11; 32]);

    fn golden_ring() -> Arc<Ring> {
        bucket_cell::testing::ring([GOLDEN_CELL])
    }

    fn golden_header() -> WalHeader {
        WalHeader {
            key_generation: 1,
            cell: GOLDEN_CELL,
            epoch: GOLDEN_EPOCH,
            segment: GOLDEN_SEG,
            salt: GOLDEN_SALT,
        }
    }

    fn golden_batch() -> WalRecord {
        WalRecord::Batch(vec![FastWalCommit {
            epoch: 1,
            tag: layout::tag(GOLDEN_EPOCH, 1),
            inline_extents: vec![(ExtentId::new(GOLDEN_CELL.id, 5), b"hello-wal".to_vec())],
            large_refs: vec![LargeRef { sequence: 6, logical_len: 262_144, bucket_segment: 0 }],
            frees: vec![ExtentId::new(GOLDEN_CELL.id, 1)],
            root: Some(b"root-v1".to_vec()),
            capacity: Some(1 << 30),
            next_sequence: 7,
            versions: vec![NewVersion { id: 0xbe, keep_last: Some(2) }],
        }])
    }

    impl formats::golden::Family for FastWalFormat {
        const FORMAT: formats::Format = FORMAT;

        fn write() -> Vec<(&'static str, Vec<u8>)> {
            let ring = golden_ring();
            let header = golden_header();
            let header_bytes = wire::encode_header(&header);
            let aead = wire::derive_aead(&header, &ring).unwrap();
            let rec_batch = wire::encode_record(&header_bytes, &aead, 0, &golden_batch()).unwrap();
            let rec_rolled =
                wire::encode_record(&header_bytes, &aead, 1, &WalRecord::Rolled).unwrap();
            let fence_off = (HEADER_LEN + rec_batch.len()) as u64;
            let rec_fence =
                wire::encode_fence(&header, &header_bytes, &ring, fence_off, GOLDEN_EPOCH + 1)
                    .unwrap();

            let mut seg_rolled = header_bytes.to_vec();
            seg_rolled.extend_from_slice(&rec_batch);
            seg_rolled.extend_from_slice(&rec_rolled);

            let mut seg_fenced = header_bytes.to_vec();
            seg_fenced.extend_from_slice(&rec_batch);
            seg_fenced.extend_from_slice(&rec_fence);

            let tomb_empty = wire::encode_tombstone(
                GOLDEN_CELL,
                GOLDEN_EPOCH,
                GOLDEN_SEG,
                &ring,
                Tombstone::EmptyFence { by_epoch: GOLDEN_EPOCH + 1 },
            )
            .unwrap()
            .to_vec();
            let tomb_rolled = wire::encode_tombstone(
                GOLDEN_CELL,
                GOLDEN_EPOCH,
                GOLDEN_SEG,
                &ring,
                Tombstone::Rolled { max_commit_epoch: 1 },
            )
            .unwrap()
            .to_vec();
            let tomb_swept = wire::encode_tombstone(
                GOLDEN_CELL,
                GOLDEN_EPOCH,
                GOLDEN_SEG,
                &ring,
                Tombstone::Swept { head_epoch: GOLDEN_EPOCH + 1 },
            )
            .unwrap()
            .to_vec();

            vec![
                ("header", header_bytes.to_vec()),
                ("segment-batch-rolled", seg_rolled),
                ("segment-batch-fence", seg_fenced),
                ("tombstone-empty-fence", tomb_empty),
                ("tombstone-rolled", tomb_rolled),
                ("tombstone-swept", tomb_swept),
            ]
        }

        fn written(version: u32, case: &str, bytes: &[u8]) -> Result<String, String> {
            match version {
                1 => Self::read_as(case, bytes),
                _ => Err(format!("no decoder of version {version}")),
            }
        }

        fn read(case: &str, bytes: &[u8]) -> Result<(), String> {
            Self::read_as(case, bytes).map(drop)
        }

        fn fingerprint(version: u32) -> Option<String> {
            match version {
                1 => Some(
                    [
                        format!("WalRecord = {}", WalRecord::fingerprint()),
                        format!("FastWalCommit = {}", FastWalCommit::fingerprint()),
                        format!("LargeRef = {}", LargeRef::fingerprint()),
                    ]
                    .join("\n")
                        + "\n",
                ),
                _ => None,
            }
        }
    }

    impl FastWalFormat {
        fn read_as(case: &str, bytes: &[u8]) -> Result<String, String> {
            let ring = golden_ring();
            let fmt = |res: Result<String, bucket_cell::Error>| res.map_err(|e| e.to_string());
            match case {
                "header" => fmt(wire::decode_header(bytes, GOLDEN_CELL, GOLDEN_EPOCH, GOLDEN_SEG)
                    .map(|(h, _)| format!("{h:#?}"))),
                "segment-batch-rolled" | "segment-batch-fence" => {
                    fmt(wire::decode_segment(bytes, GOLDEN_CELL, GOLDEN_EPOCH, GOLDEN_SEG, &ring)
                        .map(|(h, _, recs)| format!("{h:#?}\n{recs:#?}")))
                }
                "tombstone-empty-fence" | "tombstone-rolled" | "tombstone-swept" => {
                    fmt(wire::decode_tombstone(bytes, GOLDEN_CELL, GOLDEN_EPOCH, GOLDEN_SEG, &ring)
                        .map(|t| format!("{t:#?}")))
                }
                _ => Err(format!("no case {case}")),
            }
        }
    }
}
