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
//! collector, alone, deletes: what no live cell can reach, and objects it compacts.
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

pub mod batch;
pub mod bucket;
pub mod bucket_kms;
pub mod buggify;
pub mod cache;
pub mod cell;
pub mod checkpoint;
pub mod client;
pub mod clock;
pub mod collector;
pub mod direct;
pub mod evidence;
pub mod faults;
pub mod fetch;
pub mod head;
#[cfg(test)]
mod heap;
pub mod host_disk;
pub mod identity;
pub mod keys;
pub mod layout;
pub mod lifecycle;
pub mod local;
pub mod lru;
pub mod marks;
pub mod mirror;
mod moves;
pub mod object;
pub mod object_bucket;
pub mod objectid;
pub mod passed;
pub mod peer;
pub mod record;
pub mod recovery;
pub mod routed;
#[cfg(any(test, feature = "testing"))]
#[cfg(not(madsim))]
pub mod s3_server;
pub mod sweep;
pub mod trace;
pub mod wal;
pub mod writer;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use batch::{Commit, SegmentBuilder};
pub use bucket::Bucket;
/// The bytes a [`Bucket`] writes: shared, so that a write tried again holds them once.
pub use bytes::Bytes;
pub use fetch::read;
pub use layout::Location;
pub use recovery::{Recovered, State, recover};
pub use wal::{Rebased, TakenCell, Wal};
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
    /// cladefs: a cell's head found with a slot empty and no deletion's head, its maker making it,
    /// or dead making it, or failed, or the collector removing it (TASK-691): no one but its maker
    /// writes it, and whoever needs it gone fails it ([`head::fail`]).
    #[error("cell {0}'s head is being made, failed, or removed")]
    Unmade(layout::CellId),
    /// cladefs: a bucket's settings asked out of their ranges, or against the root's
    /// ([`identity::Settings`], TASK-691).
    #[error("bucket settings: {0}")]
    Settings(String),
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
    /// key unwrapped by none, or revoked; or the collector's marking, which reads none of the data
    /// of objects encrypted, its compaction reading them under their keys (TASK-411.6).
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
    /// The cell's record says its objects are of another format than this one's, newer or
    /// older, which reads none of them (doc-11, rule 1; TASK-141.2, TASK-165.3).
    #[error(
        "cell {cell} is of format {format}, which this reader, of format {ours}, doesn't read",
        ours = layout::FORMAT
    )]
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
    /// cladefs: a version's read evidence of another format than this one's, newer or older
    /// ([`evidence::FORMAT`], TASK-702.2): its reader skips it.
    #[error(
        "read evidence of format {0}, which this reader, of format {ours}, doesn't read",
        ours = evidence::FORMAT
    )]
    EvidenceFormat(u32),
    /// cladefs: a pass of the collector over a cell left it to another pass, as the cell's lease
    /// said ([`passed::CollectorLease`], TASK-395.5): nothing failed.
    #[error("the cell is left to another pass of the collector, which {0}")]
    Left(Left),
    /// cladefs: a request the bucket refuses for good, retried no more (board 5861): its
    /// credentials not let in, or the request itself refused.
    #[error("the bucket refuses {0}")]
    Refused(bucket::Refused),
    /// cladefs: the bucket's identity says another format than this one's, newer or older
    /// ([`identity`], TASK-633.9; doc-11, rule 1).
    #[error(
        "the bucket is of format {0}, which this reader, of format {ours}, doesn't read",
        ours = layout::FORMAT
    )]
    BucketFormat(u32),
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
    /// TLA+: `specs/bucket/mutations/NoHeadSwap.cfg`. cladefs: in a head of slots, the condition
    /// is on the slot the swap writes (TASK-691): `specs/head-slots/mutations/NoSlotCondition.cfg`.
    pub head_swap: bool,
    /// cladefs: a head is made with every slot ([`head::birth`], TASK-691), and no write but its
    /// maker's creates one: without it, the first slot alone, the others created as their turns
    /// come, a writer that held the cell, removed since, creates its next slot, a live head back.
    /// TLA+: `specs/head-slots/mutations/NoSlotsAtBirth.cfg`.
    pub slots_at_birth: bool,
    /// cladefs: a read of a head finds one only if it found every slot holding one, or one a
    /// deletion's (TASK-691): without it, the newest it found, though a slot is empty, a head its
    /// maker is still making, or one older than the deletion's, being removed.
    /// TLA+: `specs/head-slots/mutations/NoWholeOrFinal.cfg`.
    pub whole_or_final: bool,
    /// cladefs: a read finding a mark in a head's slot finds no head ([`head::fail`], TASK-691):
    /// without it, the newest version it found, of a head failed as its maker made it, whose other
    /// slots the maker filled.
    /// TLA+: `specs/head-slots/mutations/NoMarksFail.cfg`.
    pub marks_fail: bool,
    /// cladefs: the collector deletes a failed head's marks only a day after they were made
    /// ([`head::MARKS_KEPT`], TASK-691), its maker's creates done: without it, at once, so that the
    /// maker's late creates fill the slots the marks held, a whole head, live.
    /// TLA+: `specs/head-slots/mutations/NoMarksKept.cfg`.
    pub marks_kept: bool,
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
    /// The key of the moves a pass of the collector makes names the pass, its cell's collector
    /// epoch, so that two passes' moves of one object never share a key, and readers find them by
    /// listing the object's moves: keyed by the object alone, a pass slow to create them, after
    /// later passes made and deleted them, makes them again at their key, of other bytes
    /// (TASK-407.1).
    /// TLA+: `specs/bucket/mutations/NoMovesByPass.cfg`.
    pub moves_by_pass: bool,
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
    /// The collector keeps every moves on the way to an extent from where it was sealed, not only
    /// from where the cell's state says it is, unless that state's checkpoint is by the writer
    /// holding the cell's head, read after the checkpoint was: a checkpoint of another writer's,
    /// landing after the writer recovered the cell, or an inherited state, may name the extent
    /// further along its moves than the writer does (TASK-175).
    /// TLA+: `specs/bucket/mutations/NoWaysFromSeals.cfg`.
    pub ways_from_seals: bool,
    /// A writer names the extents its checkpoint records as moved where they went before it
    /// creates the checkpoint, not once it is created: the collector trusts a checkpoint of the
    /// writer holding the head to name nothing further along its way than the writer does
    /// (TASK-175).
    /// TLA+: `specs/bucket/mutations/NoMovesTakenFirst.cfg`.
    pub moves_taken_first: bool,
    /// A host's cell reading the extents its writer's checkpoints record as moved names them where
    /// they went, as the writer does, before the checkpoint lands
    /// ([`Writer::following`](crate::Writer::following)): it names none further back along its way
    /// than the writer's newest checkpoint, from which the collector keeps the moves (TASK-195).
    /// Simulation only (`sim::hosts_need_to_follow_moves`): specs/bucket models the writer's state,
    /// not a cell's names.
    pub hosts_follow_moves: bool,
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
}

impl Rules {
    pub const ALL: Self = Self {
        head_swap: true,
        slots_at_birth: true,
        whole_or_final: true,
        marks_fail: true,
        marks_kept: true,
        epoch_keys: true,
        collector_epochs: true,
        moves_by_pass: true,
        epoch_sequences: true,
        candidates_first: true,
        tokens: true,
        delete_tokens: true,
        forks_check_versions: true,
        forks_pending: true,
        collector_abandons: true,
        ways_from_seals: true,
        moves_taken_first: true,
        hosts_follow_moves: true,
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
