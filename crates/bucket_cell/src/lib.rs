// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: cells kept in object storage (backlog doc-7, TASK-28).
//!
//! A cell is a VM's filesystem: extents its guest wrote, in segments whose footers say what
//! each commit did, with checkpoints of its index. A writer holds the cell's head; its segments,
//! created only where none is, are numbered without gaps, and each names the one it follows, so a
//! writer taken over can add nothing to the cell once the next one's first segment, its fence,
//! is in it. Forks
//! record where they were made, and inherit the parent's extents without copying them. The
//! collector, alone, deletes: what no live cell can reach. It compacts nothing and holds no key:
//! writers relocate what their cells keep (doc-27).
//!
//! The object store is anything with conditional writes ([`Bucket`]): S3 and GCS through the
//! `object_store` crate ([`object_bucket::ObjectBucket`]) over cladefs's HTTP client ([`client`]),
//! a directory of the host's ([`local`]), and in tests a simulation that delays, reorders and fails
//! requests.

// Every unsafe block says why it is sound, other threads included (TASK-444.4).
#![deny(clippy::undocumented_unsafe_blocks)]

/// cladefs: a step of the protocol, which the failpoint of its name fails (the `fail` crate's,
/// TASK-44.10): the function it is in returns an error there, or whatever a test set the failpoint
/// to do happens. Nothing without the crate's `failpoints` feature, which `cladefs/failpoints`
/// turns on.
macro_rules! failpoint {
    ($name:literal) => {
        fail::fail_point!($name, |_| Err($crate::Error::Read(format!("failpoint {}", $name))))
    };
}

// cladefs: tests' features, which a measurement must not have: the fail crate's failpoints, whoever
// turned them on (`cladefs/failpoints`, or a dev-dependency's), each `fail_point!` of this crate's,
// local_cell's and cladefs's then looking its name up in the fail crate's registry, under its lock;
// and this crate's `testing`, whose protocol rules are a thread's, looked up at each step. A build
// without debug assertions is the release profile's, which measurements are made with; tests'
// are not (the dev and sim profiles have them). Here, as this is the lowest of the crates with
// failpoints, which every other with them depends on. A package's examples build with its
// dev-dependencies, and cladefs's own turn its failpoints on: cladefs's examples were measured with
// them, until they were cladefs-examples' (TASK-320; local_cell's audit is TASK-303).
const _: () = assert!(
    cfg!(debug_assertions) || !fail::has_failpoints(),
    "the fail crate's failpoints in a release build, which measures with them: build cladefs's \
     examples as cladefs-examples' (`-p cladefs-examples`), a package alone (`-p PACKAGE`), not across \
     the workspace, and a daemon with failpoints in the sim profile (`--profile sim`) (TASK-320)"
);
#[cfg(all(feature = "testing", not(debug_assertions)))]
compile_error!(
    "bucket_cell's `testing` in a release build, which measures with it: build the package alone \
     (`-p PACKAGE`), not across the workspace, whose dev-dependencies turn it on (TASK-320)"
);

#[cfg(feature = "full")]
pub mod any_bucket;
#[cfg(feature = "full")]
pub mod batch;
pub mod bucket;
#[cfg(feature = "full")]
pub mod bucket_kms;
#[cfg(feature = "full")]
pub mod buggify;
#[cfg(feature = "full")]
pub mod cache;
pub mod cell;
pub mod checkpoint;
#[cfg(feature = "full")]
pub mod client;
#[cfg(feature = "full")]
pub mod clock;
pub mod collector;
#[cfg(feature = "full")]
pub mod direct;
pub mod events;
pub mod evidence;
#[cfg(feature = "full")]
pub mod faults;
#[cfg(feature = "full")]
pub mod fetch;
pub mod gates;
pub mod gc_metrics;
#[cfg(all(test, feature = "full"))]
mod golden;
pub mod head;
#[cfg(all(test, feature = "full"))]
mod heap;
#[cfg(feature = "full")]
pub mod host;
#[cfg(feature = "full")]
pub mod host_disk;
#[cfg(target_arch = "wasm32")]
use getrandom as _;
#[cfg(feature = "full")]
pub use host::Host;

pub mod identity;
#[cfg(feature = "full")]
pub mod keys;
#[cfg(not(feature = "full"))]
pub mod keys {
    //! Minimal keyless types used by `Head`, `head`, and `recovery` in pure-Rust `keyless-gc`
    //! builds without `aws-lc` or KMS backends.
    use crate::Error;
    use crate::layout::CellId;
    use extent_frame::{KeyId, Keys, Secret};
    use extent_keys::Wrapped;
    use fprint::TypeFingerprint;
    use serde::{Deserialize, Serialize};
    use std::sync::Arc;

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
    pub struct Kek {
        pub generation: u32,
        pub wrapped: Wrapped,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
    pub struct Dek {
        pub generation: u32,
        pub kek: u32,
        pub wrapped: Wrapped,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
    pub struct InheritedDek {
        pub cell: CellId,
        pub dek: Dek,
    }

    #[derive(Debug, Default)]
    pub struct Ring;

    pub struct RingView;

    impl Keys for RingView {
        fn dek(&self, _id: KeyId) -> Option<&Secret> {
            None
        }
    }

    impl Ring {
        pub fn keyless() -> Arc<Self> {
            Arc::new(Self)
        }

        pub fn is_empty(&self) -> bool {
            true
        }

        pub fn of(&self, _cell: u64) -> RingView {
            RingView
        }
    }

    pub fn shredded(cell: CellId) -> Error {
        Error::Kms(format!("cell {cell} was shredded"))
    }
}
pub mod layout;
pub mod lifecycle;
#[cfg(feature = "full")]
pub mod local;
#[cfg(feature = "full")]
pub mod lru;
#[cfg(feature = "full")]
pub mod mirror;
#[cfg(not(feature = "full"))]
pub mod mirror {
    //! Minimal mirror trait for `Bucket::mirrors` in keyless single-bucket GC builds.
    use crate::Error;
    use crate::bucket::{BucketFuture, BucketSend, BucketSync, Refused};
    use crate::layout::CellId;

    pub trait Here: BucketSend + BucketSync {
        fn marked(&self, parent: CellId, at: u64) -> BucketFuture<'_, Result<bool, Error>>;
        fn create<'a>(
            &'a self,
            key: &'a str,
            bytes: Vec<u8>,
        ) -> BucketFuture<'a, Result<(), Refused>>;
        fn retire<'a>(&'a self, keys: &'a [&'a str]) -> BucketFuture<'a, Result<usize, Refused>>;
    }
}
pub mod object;
#[cfg(feature = "full")]
pub mod object_bucket;
pub mod objectid;
pub mod passed;
#[cfg(feature = "full")]
pub mod peer;
pub mod record;
pub mod recovery;
#[cfg(feature = "full")]
pub mod routed;
#[cfg(all(feature = "full", any(test, feature = "testing")))]
#[cfg(not(madsim))]
pub mod s3_server;
#[cfg(feature = "full")]
pub mod sweep;
#[cfg(feature = "full")]
pub mod trace;
#[cfg(feature = "full")]
pub mod wal;
#[cfg(feature = "full")]
pub mod writer;

#[cfg(all(feature = "full", any(test, feature = "testing")))]
pub mod testing;

#[cfg(feature = "full")]
pub use batch::{Commit, SegmentBuilder};
pub use bucket::Bucket;
/// The bytes a [`Bucket`] writes: shared, so that a write tried again holds them once.
pub use bytes::Bytes;
#[cfg(feature = "full")]
pub use fetch::read;
pub use layout::Location;
pub use recovery::{Recovered, State, recover};
#[cfg(feature = "full")]
pub use wal::{Rebased, TakenCell, Wal};
#[cfg(feature = "full")]
pub use writer::{Body, Objects, Writer};

/// Extent IDs' sequence numbers carry the epoch of the writer that minted them above this bit,
/// so that a writer never mints an ID one before it minted, even one whose upload landed after it
/// took over.
pub const EPOCH_SHIFT: u32 = 40;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Another writer took the cell over.
    #[error("another writer took the cell")]
    Fenced,
    #[error("the cell was deleted")]
    Deleted,
    /// A later placement, whose fencing token is higher, holds the cell's head: this one, cut off
    /// from it, takes it back no more (TASK-39.7).
    #[error("a later placement holds the cell")]
    Stale,
    #[error("no such cell")]
    NoCell,
    /// A fork's child has a record naming another origin: another cell has the ID the fork's
    /// guest gave it, which that cell took while the fork was made (TASK-79.82). The fork failed;
    /// its parent's cell is as sound as before.
    #[error("cell {0} has another origin")]
    IdTaken(layout::CellId),
    /// Something a cell needs is gone.
    #[error("lost: {0}")]
    Lost(String),
    #[error("corrupt: {0}")]
    Corrupt(String),
    /// cladefs: an object's data, encrypted under a DEK its reader doesn't hold (TASK-411.2): its
    /// key unwrapped by none, or revoked (TASK-411.6).
    #[error("{}", extent_frame::Error::NoKey(*.0))]
    NoKey(extent_frame::KeyId),
    /// cladefs: what a KMS refused, naming the tenant whose KEK was asked for (TASK-411.3): its KEK
    /// revoked or destroyed, none there, or the KMS failed.
    #[error("{0}")]
    Kms(String),
    /// cladefs: `cell` inherits from `from`, of another cell, whose objects are encrypted, and its
    /// record holds not `from`'s keys (TASK-411.4): it was given its own without them. A fork so
    /// is refused before anything of it is made (`keys::inheriting`).
    #[error(
        "cell {cell} inherits from cell {from}, of another cell, whose objects are \
         encrypted, and its record holds not their keys"
    )]
    Inherited { cell: layout::CellId, from: layout::CellId },
    /// The cell's record says its objects are of a format version this build doesn't read
    /// (doc-28, board 10038).
    #[error("cell {cell}: {}", formats::Refused { format: layout::FORMAT, found: *.format })]
    Format { cell: layout::CellId, format: u32 },
    /// What a writer was to upload, it could not read where it is kept: a cell's file.
    #[error("reading {0}")]
    Read(String),
    /// Another writer's commits moved the cell on from the host's write-ahead log's: the cell's
    /// last commit is neither one of the log's nor the one the log started from. Its commits were
    /// never acknowledged, and no writer puts them after another's: the cell opens only once
    /// `cladefsctl take` drops the log (doc-14, Opening a cell).
    #[error("another writer's commits moved the cell on from the write-ahead log's")]
    Diverged,
    /// cladefs: the cell is of another kind, this one: a cell has one (doc-20).
    #[error("the cell is of kind {0}")]
    OtherKind(&'static str),
    /// cladefs: a taking found the head's keys changed from those it made what it gives for
    /// ([`head::Given`]): another's taking gave them first. Its keys are made anew.
    #[error("another taking gave the cell its keys first")]
    Raced,
    /// cladefs: a fork of a version to a cell of another tenant's than the version's cell's, which
    /// its head doesn't say is shared ([`layout::Version::shared`], TASK-395.7), nor is the default
    /// tenant's ([`layout::DEFAULT_TENANT`]).
    #[error("cell {0}'s version is another tenant's, not shared")]
    NotShared(layout::CellId),
    /// cladefs: a reference of an ephemeral cell of a host whose incarnation, this one, was
    /// decommissioned (board 8519, [`layout::decommissioned`]): taken back, refused. The host's
    /// ephemeral cells' references are retired; it must stop forking them, and its operator
    /// start it again of a new incarnation.
    #[error("incarnation {} was decommissioned", base32_id::Short(*.0))]
    Decommissioned(u64),
    /// cladefs: a version's read evidence of a format version this build doesn't read
    /// ([`layout::FORMAT`], TASK-702.2, doc-28): its reader skips it.
    #[error("read evidence: {}", formats::Refused { format: layout::FORMAT, found: *.0 })]
    EvidenceFormat(u32),
    /// cladefs: a pass of the collector over a cell left it to another pass, as the cell's lease
    /// said ([`passed::CollectorLease`], TASK-395.5): nothing failed.
    #[error("the cell is left to another pass of the collector, which {0}")]
    Left(Left),
    /// cladefs: a request the bucket refuses for good, retried no more (board 5861): its
    /// credentials not let in, or the request itself refused.
    #[error("the bucket refuses {0}")]
    Refused(bucket::Refused),
    /// cladefs: an object in the bucket says a format version this build doesn't read
    /// ([`layout::FORMAT`], TASK-633.9, doc-28).
    #[error("the bucket: {}", formats::Refused { format: layout::FORMAT, found: *.0 })]
    BucketFormat(u32),
    /// cladefs: a requested feature requires a newer storage format version than this bucket's
    /// active ratchet gate allows (doc-28): ratchet the bucket to enable it.
    #[error(
        "{family} feature requires version {needed}, but this bucket is gated at version {gated}; \
         ratchet the bucket with `cladefsctl ratchet` once all nodes are upgraded"
    )]
    FormatGated { family: &'static str, needed: u32, gated: u32 },
    /// cladefs: the bucket's `formats` ratchet object is corrupt, incompatible with this build, or
    /// rejected a requested ratchet update (doc-28).
    #[error(transparent)]
    Gates(#[from] formats::GateError),
    /// cladefs: a bucket to add is a bucket already, of this number ([`identity::add`],
    /// TASK-633.9).
    #[error("the bucket is bucket {0} already")]
    Formatted(u16),
    /// cladefs: buckets are added to the root bucket alone, not to this one, of this number
    /// ([`identity::add`]).
    #[error("bucket {0} is not the root bucket, which adds buckets")]
    NotRoot(u16),
    /// cladefs: every bucket number is given ([`identity::add`]).
    #[error("every bucket number is given")]
    NoNumbers,
    /// cladefs: no registry of buckets this host reads records the bucket of this number
    /// (TASK-633.3).
    #[error("no bucket {0} is recorded")]
    NoBucket(u16),
    /// cladefs: the bucket the registry records as this number's is another: of the number found
    /// in its identity, or none (TASK-633.3).
    #[error("the bucket recorded as bucket {number} is {}", match found {
        Some(found) => format!("bucket {found}, or another bucket {number}"),
        None => "no bucket".to_owned(),
    })]
    Mismatched { number: u16, found: Option<u16> },
}

/// cladefs: why a pass of the collector left its cell to another ([`Error::Left`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Left {
    /// Another pass holds the cell's lease, unexpired.
    #[error("holds it")]
    Held,
    /// Another pass over the cell ended since the pass's sweep began.
    #[error("went over it since this sweep began")]
    Swept,
}

impl From<extent_frame::Corrupt> for Error {
    fn from(corrupt: extent_frame::Corrupt) -> Self {
        Self::Corrupt(corrupt.0)
    }
}

impl From<extent_frame::Error> for Error {
    fn from(error: extent_frame::Error) -> Self {
        match error {
            extent_frame::Error::Corrupt(corrupt) => corrupt.into(),
            extent_frame::Error::Format(refused) => Self::BucketFormat(refused.found),
            extent_frame::Error::NoKey(key) => Self::NoKey(key),
        }
    }
}

/// The rules doc-7's protocol relies on, each of which the simulation shows to be needed by
/// turning it off, and specs/bucket's TLA+ by its mutation removing it (doc-7, Testing).
#[derive(Clone, Copy, Debug)]
pub struct Rules {
    /// A writer acknowledges what it uploaded only by swapping the cell's head to name it, with
    /// the condition that the head is its own taking still ([`head::swap`]): written whatever is
    /// there, a writer taken over writes the head over its successor's, whose acknowledged commits
    /// it then names not (TASK-407).
    /// TLA+: `specs/bucket/mutations/NoHeadSwap.cfg`.
    pub head_swap: bool,
    /// A segment's and a checkpoint's key names its writer's epoch ([`layout::segment`],
    /// [`layout::checkpoint`]): without it, two writers' segments of one number land at one key,
    /// the later over the earlier, which a head may name (TASK-407).
    /// TLA+: `specs/bucket/mutations/NoEpochKeys.cfg`.
    pub epoch_keys: bool,
    /// The collector keeps, of a live cell's objects its head does not name, those of the head's
    /// epoch it may yet name: segments and extents of their own at or past its end, checkpoints
    /// newer than its own; it deletes the rest, of epochs no swap of whose can succeed (TASK-407).
    /// TLA+: `specs/bucket/mutations/NoCollectorEpochs.cfg`.
    pub collector_epochs: bool,
    /// A writer mints extents from its epoch's range of sequence numbers, having checked, after
    /// recovering the cell, that the head is still its taking.
    /// TLA+: `specs/bucket/mutations/NoEpochSequences.cfg`.
    pub epoch_sequences: bool,
    /// A fork's child gets its record once the segment holding the fork is acknowledged.
    /// TLA+: `specs/bucket/mutations/NoRecordsAfterAck.cfg`.
    #[cfg_attr(not(test), allow(dead_code))]
    /// The collector lists objects before it reads what cells need.
    /// TLA+: `specs/bucket/mutations/NoCandidatesFirst.cfg`.
    pub candidates_first: bool,
    /// A writer takes a cell's head only with a fencing token as high as the head's.
    /// TLA+: `specs/bucket/mutations/NoTokens.cfg`.
    pub tokens: bool,
    /// A cell is deleted only with a fencing token as high as its head's (TASK-45.1).
    /// TLA+: `specs/bucket/mutations/NoDeleteTokens.cfg`.
    pub delete_tokens: bool,
    /// A fork of a version reads its cell's head after it creates its own record: a version
    /// dropped before that fails it, its pass of the collector not having seen the child.
    /// TLA+: `specs/bucket/mutations/NoForksCheckVersions.cfg`.
    pub forks_check_versions: bool,
    /// A fork of a version's record says it is being made until that read found the version named,
    /// so that what never was made holds up no pass of the collector, whatever it names.
    /// TLA+: `specs/bucket/mutations/NoForksPending.cfg`.
    pub forks_pending: bool,
    /// The collector gives up on the forks being made of versions dropped.
    /// TLA+: `specs/bucket/mutations/NoCollectorAbandons.cfg`.
    pub collector_abandons: bool,
    /// A host's write-ahead log is put after the cell's end only if the cell's last commit is
    /// one of the log's or the one it started from: otherwise another writer's commits came after
    /// what the log holds, and it diverged (doc-14, Opening a cell).
    /// TLA+: `specs/bucket/mutations/NoWalFollows.cfg`.
    pub wal_follows: bool,
    /// cladefs: a fork of a cell to a child in another cell, a writable fork or a version's clone,
    /// has a reference under the parent's cell ([`layout::reference`]), for which that cell's
    /// passes keep the parent's state at the origin: a pass lists nothing of the child's cell, and,
    /// the parent deleted, would delete what the child inherits (TASK-395.3). TLA+:
    /// `specs/bucket/mutations/NoReferences.cfg`.
    pub references: bool,
    /// A version's clone writes its reference before it reads its cell's head (and after its own
    /// record, [`Rules::clones_record_first`]), whatever its cell, its origin's own too
    /// ([`lifecycle::fork_version`]): one that read the version named then wrote it before the
    /// version's drop, which a pass reading the version dropped read after.
    /// TLA+: `specs/bucket/mutations/NoForksReferFirst.cfg`.
    pub clones_refer_first: bool,
    /// A reference is retired, by a pass over its child's cell, only once that cell holds no live
    /// cell and no reference of its own: the child's forks inherit what it inherited, kept by its
    /// reference (TASK-395.4).
    /// TLA+: `specs/bucket/mutations/NoReferencesHeld.cfg`.
    pub references_held: bool,
    /// A pass lists its cell's references after it read the states, as it lists the records
    /// again, not with its candidates: listed then, it misses a clone's of a version a checkpoint
    /// of the version's cell dropped since (TASK-395.4).
    /// TLA+: `specs/bucket/mutations/NoReferencesRelisted.cfg`.
    pub references_relisted: bool,
    /// A pass keeps, of the parent's state at a referenced origin, what recovering it reads,
    /// not only the objects holding its extents: a child with no checkpoint of its own recovers
    /// through it (TASK-395.4).
    /// TLA+: `specs/bucket/mutations/NoForkPointsKept.cfg`.
    pub origins_kept: bool,
    /// A version's clone creates its record, saying it is being made, before its reference
    /// ([`lifecycle::fork_version`]): a reference then always has a record naming its origin, by
    /// which its child's cell's pass retires it, the clone's host dying or not; and a clone refused
    /// by a record there writes none. The other way round, a host dying between the two leaves a
    /// reference no record names, which no pass retires (TASK-395.10). TLA+:
    /// `specs/bucket/mutations/NoForksRecordFirst.cfg`.
    pub clones_record_first: bool,
    /// A reference retired, or taken back, is marked first ([`layout::retired`]), the mark kept for
    /// good under the parent's cell, and a pass over that cell deletes the references it lists
    /// marked: a writer whose read of the child's record came before, finding none, writes it
    /// after, which no record names and no recorder looks at again (TASK-395.13), or which the
    /// child's cell's passes, skipping that cell, dead and unchanged, never retire again
    /// (TASK-395.14). Its write changes the parent's cell's listing, whose next pass finds it.
    /// TLA+: `specs/bucket/mutations/NoRetirementsMarked.cfg`.
    pub retirements_marked: bool,
    /// cladefs: a pass keeps, for each version a live cell's head names, the cell's whole state
    /// at it and what recovering it reads, as for an origin (doc-18, TASK-476). Without it, as a
    /// test shows it needed, a checkpoint since covers the version's segments, which the pass
    /// deletes, and the version recovers not.
    /// TLA+: `specs/bucket/mutations/NoVersionsKept.cfg`.
    pub versions_kept: bool,
    /// cladefs: a takeover's swap of a cell's head names the versions it named
    /// ([`head::take_from`]), as it names its segments. Without it, as a test shows it needed, they
    /// are gone though no writer dropped them.
    /// TLA+: `specs/bucket/mutations/NoVersionsCarried.cfg`.
    pub versions_carried: bool,
    /// cladefs: a writer's head names a version only with the segment cutting it, as its
    /// acknowledgement names that segment ([`writer::Writer::acknowledge`]). Without it, as a test
    /// shows it needed, an acknowledgement names every version taken, those of segments it doesn't
    /// name among them, at a segment the head does not hold.
    /// TLA+: `specs/bucket/mutations/NoVersionsAcked.cfg`.
    pub versions_acked: bool,
    /// cladefs: a take of a replicated cell opened holding more than the members of
    /// the configuration it fenced gave, its own write-ahead log's commits, activates its own
    /// configuration only by the acknowledgement that names them all (`local_cell::upload`'s
    /// `Shared::replicate`, TASK-678): without it, the head names a configuration none of whose
    /// members hold commits told replicated, and the next take, elsewhere, loses them.
    /// TLA+: `specs/replication/mutations/InstallMembersOnly.cfg`.
    pub installs_own: bool,
    /// cladefs: the collector keeps large own-extents of a live cell's active fast WAL epoch
    /// (`head.fast_wal.active.epoch`) at or past `head.end`, even after a takeover bumped
    /// `head.epoch` past it, until the taker flushes the fast WAL and switches `active` (doc-29).
    /// TLA+: `specs/fast-wal/mutations/GcDeletesWalExtents.cfg`.
    pub protect_wal_epoch: bool,
    /// cladefs: a take of a cell with an active fast WAL fences each of the old epoch's segments
    /// it ends its walk at, an open one with a `Fence` record and a missing one with an empty
    /// fence tombstone, before it recovers them (`local_cell::fast_wal::fence_and_recover`,
    /// doc-29, board 14433): without it, the writer taken over appends on, its commits told
    /// durable, past what the take recovered.
    /// TLA+: `specs/fast-wal/mutations/NoFence.cfg`.
    pub wal_fences: bool,
    /// cladefs: a fast WAL writer appends only at the offset where it last left its segment,
    /// halting on a mismatch (`Bucket::append`): without it, a writer that finds its segment
    /// longer re-reads its length and appends there, past a taker's fence or tombstone.
    /// TLA+: `specs/fast-wal/mutations/NoOffsetCheck.cfg`.
    pub wal_offset_checked: bool,
    /// cladefs: a fast WAL writer begins its next segment only once its `Rolled` record on the
    /// segment before landed: without it, a taker fences the segment open, ends its walk there,
    /// and loses what the writer appends to the next.
    /// TLA+: `specs/fast-wal/mutations/NoRollBeforeNext.cfg`.
    pub wal_rolls_before_next: bool,
    /// cladefs: a take walks on past a rolled segment, a `Rolled` record's or a rolled tombstone's,
    /// to the next: without it, it ends its walk at a rolled segment and loses the next's commits.
    /// TLA+: `specs/fast-wal/mutations/NoFenceFollowsRoll.cfg`.
    pub wal_fence_follows_roll: bool,
    /// cladefs: a fast WAL writer rolls its segment before the store's limit of appends to an
    /// object, keeping room for its `Rolled` record and a taker's `Fence`: without it, a take
    /// finds a segment full, open, that it can fence never.
    /// TLA+: `specs/fast-wal/mutations/NoRollBeforeLimit.cfg`.
    pub wal_rolls_before_limit: bool,
    /// cladefs: a take's swap of a cell's head keeps `head.fast_wal.active` as it was, naming the
    /// old epoch's fast WAL, until the taker's activation replaces it ([`head::take_from`]):
    /// without it, the head forgets the fast WAL holding commits told durable, which nobody then
    /// fences or recovers.
    /// TLA+: `specs/fast-wal/mutations/ClearWalOnTake.cfg`.
    pub wal_kept_on_take: bool,
    /// cladefs: a take names its own fast WAL in the head only in the swap acknowledging what it
    /// recovered of the old one: without it, a take that stops between the two leaves a head
    /// naming the new fast WAL, the old one's commits unrecovered and forgotten.
    /// TLA+: `specs/fast-wal/mutations/ActivateBeforeFlush.cfg`.
    pub wal_activates_after_flush: bool,
    /// cladefs: a fast WAL writer appends a batch referring to large extents only once their
    /// uploads to the cold bucket landed: without it, a take recovers a commit whose extents are
    /// in no bucket.
    /// TLA+: `specs/fast-wal/mutations/WalBeforeLarge.cfg`.
    pub wal_after_large: bool,
    /// cladefs: a fast WAL writer reclaims a rolled segment only once the cold bucket's head
    /// acknowledged every commit in it: without it, a take finds commits told durable in neither.
    /// TLA+: `specs/fast-wal/mutations/DeleteWalBeforeAck.cfg`.
    pub wal_reclaims_after_ack: bool,
    /// cladefs: a sweep of superseded fast WAL segments overwrites each with a tombstone, never
    /// deleting it while its cell lives: without it, a writer taken over recreates a deleted
    /// segment by a create landing late, its commits told durable after the take.
    /// TLA+: `specs/fast-wal/mutations/SweepDeletesFence.cfg`.
    pub wal_sweep_tombstones: bool,
    /// cladefs: a fast WAL writer reclaims a rolled segment by overwriting it with a rolled
    /// tombstone, never deleting it: without it, a take finds the segment missing, fences it
    /// empty, and ends its walk before the segments after it.
    /// TLA+: `specs/fast-wal/mutations/DeleteClearsRolled.cfg`.
    pub wal_rolled_tombstones: bool,
    /// cladefs: a fast WAL writer whose fast bucket fails gives up on a batch only once the cold
    /// bucket's head acknowledged all of it: without it, a batch dropped leaves a gap, which the
    /// next batch's durability covers.
    /// TLA+: `specs/fast-wal/mutations/WalSkipsUnacked.cfg`.
    pub wal_contiguous: bool,
}

impl Rules {
    pub const ALL: Self = Self {
        head_swap: true,
        epoch_keys: true,
        collector_epochs: true,
        epoch_sequences: true,
        candidates_first: true,
        tokens: true,
        delete_tokens: true,
        forks_check_versions: true,
        forks_pending: true,
        collector_abandons: true,
        wal_follows: true,
        references: true,
        clones_refer_first: true,
        references_held: true,
        references_relisted: true,
        origins_kept: true,
        clones_record_first: true,
        retirements_marked: true,
        versions_kept: true,
        versions_carried: true,
        versions_acked: true,
        installs_own: true,
        protect_wal_epoch: true,
        wal_fences: true,
        wal_offset_checked: true,
        wal_rolls_before_next: true,
        wal_fence_follows_roll: true,
        wal_rolls_before_limit: true,
        wal_kept_on_take: true,
        wal_activates_after_flush: true,
        wal_after_large: true,
        wal_reclaims_after_ack: true,
        wal_sweep_tombstones: true,
        wal_rolled_tombstones: true,
        wal_contiguous: true,
    };
}

/// The rules the protocol keeps: all of them (cladefs: public, as `local_cell` keeps one too).
#[cfg(not(any(test, feature = "testing")))]
pub fn rules() -> Rules {
    Rules::ALL
}

#[cfg(any(test, feature = "testing"))]
thread_local! {
    static RULES: std::cell::Cell<Rules> = const { std::cell::Cell::new(Rules::ALL) };
}

/// The rules this thread's protocol keeps: all of them, but in tests that turn some off
/// ([`set_rules`]).
#[cfg(any(test, feature = "testing"))]
pub fn rules() -> Rules {
    RULES.with(|rules| rules.get())
}

/// Has this thread's protocol keep `rules` alone, as tests that see a rule needed do: bucket_cell's
/// own, and local_cell's simulation of the protocol (the `testing` feature).
#[cfg(any(test, feature = "testing"))]
pub fn set_rules(rules: Rules) {
    RULES.with(|cell| cell.set(rules));
}
