// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Where cells keep what, in a bucket, and what the objects hold (doc-7, Layout).
//!
//! Numbers in keys are fixed-width base 32 ([`Padded`]), so keys list in numeric order. Objects are
//! read from a bucket, by ranged reads, as [`crate::object`] reads them.

use base32_id::Padded;
use bincode::Options as _;
use fprint::TypeFingerprint;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use storage_device::ExtentId;

pub use extent_frame::segment::{
    CellId, Chain, Contents, Continued, Footer, Location, NewVersion, epoch_of, tag, writer_of,
};

/// cladefs: a fork's child: its cell, and its owner, which every record of the child names,
/// whoever writes it (TASK-79.82, TASK-153.9).
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    TypeFingerprint,
)]
pub struct Child {
    pub cell: CellId,
    pub owner: u64,
}

/// cladefs: the format of a cell's objects, which its record says first
/// ([`Record::format`](crate::record::Record::format)): a reader refuses a cell of any other, newer
/// or older, naming it, and reads none of it (doc-11, rule 1; TASK-141.2). 2: a footer holds its
/// segment's [`Contents`], with the capacity its commits set, and a checkpoint the cell's capacity
/// (TASK-165.3); nothing reads 1's, as nothing keeps any (AGENTS.md). 3: a commit may span
/// segments, a footer naming the runs in its data of the commit it goes on with
/// (`Contents::continued`, TASK-92.41); nothing reads 2's. 4: a checkpoint names the cell's fences,
/// which the collector keeps ([`State::fences`](crate::recovery::State::fences), TASK-371); nothing
/// reads 3's. 5: a cell's objects are under its tenant's cell's prefix, and footers, forks and
/// origins name cells with their cells ([`CellId`], TASK-364); nothing reads 4's. 6: a checkpoint's
/// fences count the cell's strays (TASK-383); nothing reads 5's, whose fences lack them. 7: a
/// checkpoint names the cell's strays apart from its fences, each void or late
/// ([`Stray`](crate::recovery::Stray), TASK-388); nothing reads 6's. 8: a version's record says
/// whether other tenants may fork it ([`Record::shared`](crate::record::Record::shared),
/// TASK-395.7); nothing reads 7's. 9: everything a cell's collection reads is under the cell's
/// prefix: its objects' moves, its collector's lease and marks ([`collector_lease`];
/// marks went, board 10545), and the references of forks of its cells ([`reference`]), so that a pass of the
/// collector lists the cell alone (TASK-395.2); nothing reads 8's, whose moves and collector's
/// lease were at the bucket's root. 10: a cell's object keeps its KEKs, wrapped by its tenant's in
/// a KMS, and a record its cell's DEKs, wrapped by its cell's ([`crate::keys`], doc-16,
/// TASK-411.3); objects are framed in extent_frame's FORMAT 2, encrypted under their cell's DEK
/// (none in the clear since TASK-411.28, which changed no format), own extents' footers naming
/// their extents, and moves' the object they are the moves of; nothing reads 9's. 11: a key is
/// written once (TASK-407): segments' and checkpoints' keys name their writers' epochs, a cell's
/// head ([`head`], [`Head`]) names the segments it holds and its newest checkpoint, replacing its
/// lease, and footers name no segment they follow; nothing reads 10's. 12: a fork of a version,
/// made, says so ([`Made::Clone`](crate::record::Made::Clone)), so that the collector removes a
/// dead cell of given roots and clones whole (TASK-455); nothing reads 11's. 13: a fork of a
/// version says whether another than its host resumed it ([`Head::resumed`]), whose clone's
/// reference is retired marked (TASK-455's review); nothing reads 12's. 14: a cell's head names its
/// versions, and a footer those made at its end ([`Head::versions`], [`NewVersion`]; doc-18,
/// TASK-476); a fork being made says whether it is of a version or a version; nothing reads 13's.
/// 15: every number in a key is in base 32 ([`Padded`], TASK-504), as a [`CellId`] is displayed in
/// its records; nothing reads 14's, whose keys were in hexadecimal. 16: a cell has one kind
/// (doc-20, TASK-508.1): its object names its one cell and kind, not a map of cells, and a version
/// names no cut of several cells; nothing reads 15's. 17: forks are of versions alone: a record
/// names no version, nor a version it is being forked from (TASK-508.2); nothing reads 16's. 18: no
/// writable forks: a record is its kind, not flags, and a footer names no forks (TASK-508.3);
/// nothing reads 17's. 19: a cell's head is its one swapped object, holding what its record and its
/// cell's object held (TASK-515); nothing reads 18's. 20: a bucket has a number, in its identity
/// object ([`IDENTITY`]), which the high bits of every cell ID it gives carry ([`crate::identity`],
/// doc-23, TASK-633.9); nothing reads 19's. 21: a cell's head says whether it is replicated, and
/// names its active configuration ([`Head::replication`], doc-21, TASK-537.3.1); nothing reads
/// 20's. 22: a version forked into another bucket is mirrored there (doc-23, Mirrors; TASK-633.8):
/// its pin, a reference of the mirror's own ([`pin`]), and its marker ([`mirror`]), under the
/// parent's prefix in that bucket; nothing reads 21's. 23: a cell's head is in its bucket's count
/// of objects, its slots, each written in turn (TASK-691), as GCS takes
/// about one write a second of an object, and a bucket's identity holds its settings;
/// nothing reads 22's, whose head was one object. 24: a version's
/// read evidence, what forks of it read of it as they opened, each tenant's under the version's
/// cell's prefix ([`evidence`], [`crate::evidence`], TASK-702.2); nothing reads 23's. 25: an origin
/// and a version name the epoch of their segment, whose checkpoint is read by its key, with no read
/// of its cell's head (board 6017); nothing reads 24's. 26: a checkpoint keeps the length of the
/// data of each of its cell's own segments its extents are in ([`crate::State::lengths`], doc-27),
/// which its writer measures their density against; nothing reads 25's. 27: an ephemeral cell's
/// index entry is kept, and routed, in its reference's bucket, which its key names, beside its
/// host's incarnation's tombstone ([`ephemeral_index`], [`decommissioned`], boards 8517, 8519);
/// nothing reads 26's, whose entries were by incarnation alone, in the bucket of the daemon that
/// wrote them, which no decommissioning would find. 28: the collector compacts nothing, its writers
/// relocating out of their sparse segments (doc-27 step 2): no moves; nothing reads 27's. 29: a
/// cell's head is one object, swapped in place (doc-27 step 4): no slots, no marks, and a bucket's
/// identity holds no settings; a cell holding the head slots of 28's is refused ([`Kind::Former`]);
/// nothing reads 28's. 30: its
/// framed objects are extent frames of version 3, their version a number (board 10040); nothing
/// reads 29's, whose frames were of 2.
/// 31, frozen (doc-28, board 10038): every bucket object kind says its version (segment footers,
/// own-extent footers, checkpoint tables/states/appendices, objectid blocks, read evidence, heads,
/// collector leases, identity, registry, references, retired marks and mirrors), readers check by
/// range ([`formats::Format::check`]) and refuse trailing bytes, and bucket numbers in keys
/// ([`ephemeral_hosts`], [`hold`]) are fixed-width base 32 ([`Padded`]). Every later build reads
/// every version from 31 on.
/// 32: `Fsync::Local` removed from `Head`'s replication wish (`Ordered` and `Replicated` alone);
/// version 31 heads carrying `v31::Fsync::Local` decode as [`Fsync::Ordered`] with a warning.
/// 33 (board 11699): per-run write-once mirror pin (`refs/<at>/mirror-pins/<run>`) and hold
/// (`refs/<at>/mirror-holds/<B>/<run>`) keys added under `/`-terminated prefixes so `releasesOwn`
/// holds by key disjointness under plain unconditional `DELETE` (deleting `Bucket::delete_if`)
/// without colliding with legacy files on `file://` buckets; legacy v31/v32 single-key pins
/// (`refs/<at>/mirror`) and holds (`refs/<at>/mirror.<B>`) still decode as [`Kind::Pin`] and
/// [`Kind::Hold`] and are cleaned up by the collector.
/// 34 (board 9498): [`Mirror`] carries [`MirrorState`] (`Copying { owner }` or `Marked`) so a
/// mirror claims `mirrors/<at>` in `Copying` before copying and transitions it conditionally
/// (`PutMode::Update`) to `Marked`, preventing a late marker write from marking a collected or
/// taken-over copy; v31..=v33 markers decode as [`MirrorState::Marked`].
/// 35: Fast WAL commit mode (doc-29): [`Fsync::FastWal`], [`Head::fast_wal`] ([`FastWal`],
/// [`ActiveWal`]), and per-epoch fast WAL keys ([`wal`], [`Kind::Wal`]); version 32, 33, and 34
/// heads decode with `fast_wal: None`.
pub const FORMAT: formats::Format = formats::Format::new("bucket layout", 31, 35);

/// cladefs: the prefix of every tenant's cells' objects: all the bucket's cells (TASK-364).
pub const TENANTS: &str = "tenants/";
/// cladefs: under a cell's prefix, the collector's own: its lease, and what else its passes over
/// the cell keep there; never a pass's candidate (TASK-395.2).
const GC: &str = "gc/";
const GC_LEASE: &str = "gc/lease";
/// Under a cell's prefix, in formats before 29, the slots of its head ([`Kind::Former`]).
const HEAD_SLOTS: &str = "head/";
/// cladefs: under a cell's prefix, the references of forks of its cells ([`reference`]).
const REFS: &str = "refs/";
/// cladefs: under a cell's prefix, the marks of the references retired or taken back ([`retired`]).
const RETIRED: &str = "retired/";
/// cladefs: under a cell's prefix in another bucket, the markers of its versions mirrored there
/// ([`mirror`], TASK-633.8).
const MIRRORS: &str = "mirrors/";
/// cladefs: the legacy v31/v32 name of a mirror's pin key and hold prefix ([`legacy_pin`]).
const PIN: &str = "mirror";
/// cladefs: v33 `/`-terminated prefix of per-run mirror pins under `refs/<at>/` ([`pins`]).
const MIRROR_PINS: &str = "mirror-pins/";
/// cladefs: v33 `/`-terminated prefix of per-run mirror holds under `refs/<at>/` ([`holds`]).
const MIRROR_HOLDS: &str = "mirror-holds/";
/// cladefs: where an ephemeral cell's reference names its host's incarnation, before its child
/// ([`ephemeral_reference`], doc-25); and, at the bucket's root, the prefix of the index of those
/// references by bucket and incarnation ([`ephemeral_index`]) and of incarnations' tombstones
/// ([`decommissioned`]).
const EPHEMERAL: &str = "ephemeral";
/// cladefs: the last word of an incarnation's tombstone ([`decommissioned`], board 8519).
const DECOMMISSIONED: &str = "decommissioned";
/// cladefs: under a cell's prefix, the read evidence of its versions ([`evidence`], TASK-702.2).
const READS: &str = "reads/";
/// cladefs: under a cell's prefix, fast WAL segments in an appendable zonal bucket ([`wal`],
/// doc-29).
const WAL: &str = "wal/";
/// cladefs: the prefix of the blocks of object IDs taken ([`objectid_block`]).
const OBJECTIDS: &str = "objectid/";
/// cladefs: the bucket's identity object: its format and its number, written once by its formatting
/// ([`crate::identity`], TASK-633.9). No cell's, and no candidate of any pass.
pub const IDENTITY: &str = "identity";
/// cladefs: the record of every bucket, the root bucket's own or a copy of it ([`crate::identity`],
/// TASK-633.9). No cell's, and no candidate of any pass.
pub const BUCKETS: &str = "buckets";
/// cladefs: the global control-plane event log and durable state in the root bucket
/// ([`crate::events`]): a single protobuf object polled via `If-None-Match` by extent daemons and
/// read by the collector. No cell's, and no candidate of any pass.
pub const EVENTS: &str = "events";
/// cladefs: the bucket's monotonic storage-version upgrade ratchet ([`crate::gates`], doc-28):
/// written at format time in each bucket (`identity::format`, `identity::add`) and advanced
/// monotonically by `cladefsctl ratchet`. No cell's, and no candidate of any pass.
pub const FORMATS: &str = "formats";

pub fn segments(cell: CellId) -> String {
    format!("{}segments/", cell.prefix())
}

/// Segment `segment` of `cell` by the writer of epoch `epoch`: `segment` counts from 0 without
/// gaps, and a writer writes each of its numbers once, so that no key is written twice; the segment
/// is the cell's once its head names it ([`Head`], TASK-407). The epoch first, as an extent's
/// key has it: a writer's keys share all but their numbers, which a checkpoint's table, keeping
/// keys front-coded, stores once.
pub fn segment(cell: CellId, segment: u64, epoch: u64) -> String {
    format!("{}{}.{}", segments(cell), Padded(keyed(epoch)), Padded(segment))
}

/// The epoch a key names: its writer's (rule `epoch_keys`); without the rule, none, every writer's
/// objects of one number at one key, as a test shows it needed.
fn keyed(epoch: u64) -> u64 {
    if crate::rules().epoch_keys { epoch } else { 0 }
}

pub fn extents(cell: CellId) -> String {
    format!("{}extents/", cell.prefix())
}

/// The object of an extent of `cell`'s stored on its own, uploaded by the writer of epoch `epoch`
/// before its segment `segment`, which names it.
pub fn extent(cell: CellId, epoch: u64, segment: u64, extent: ExtentId) -> String {
    extent_frame::segment::extent_key(cell, epoch, segment, extent)
}

pub fn checkpoints(cell: CellId) -> String {
    format!("{}checkpoints/", cell.prefix())
}

/// The checkpoint of `cell` as of its segment `segment`, by the writer of epoch `epoch`: the
/// cell's newest once its head names it ([`Head::checkpoint`], TASK-407).
pub fn checkpoint(cell: CellId, segment: u64, epoch: u64) -> String {
    format!("{}{}.{}", checkpoints(cell), Padded(keyed(epoch)), Padded(segment))
}

/// The head of `cell` ([`Head`]): what made the cell, its keys, its writer's taking, the segments
/// it holds, its newest checkpoint and its versions, one object swapped in place with a condition
/// (TASK-407, TASK-515, doc-27).
pub fn head(cell: CellId) -> String {
    format!("{}head", cell.prefix())
}

/// cladefs: the prefix of `cell`'s fast WAL objects in its appendable zonal bucket (doc-29).
pub fn wal_prefix(cell: CellId) -> String {
    format!("{}{WAL}", cell.prefix())
}

/// cladefs: the prefix of `cell`'s fast WAL segments for writer epoch `epoch` (doc-29).
pub fn wal_epoch_prefix(cell: CellId, epoch: u64) -> String {
    format!("{}{}/", wal_prefix(cell), Padded(epoch))
}

/// cladefs: fast WAL segment `segment` of `cell` for writer epoch `epoch` (doc-29):
/// `tenants/<T>/cells/<C>/wal/<epoch>/<segment>`, created at byte offset 0 and appended to via
/// offset-checked appends until rolled or fenced.
pub fn wal(cell: CellId, epoch: u64, segment: u64) -> String {
    format!("{}{}", wal_epoch_prefix(cell, epoch), Padded(segment))
}

/// cladefs: the block of object IDs `block` taken, by one allocator of one process's
/// ([`crate::objectid`], TASK-364): there once taken, and kept for good, so that no other takes
/// it, and none of its IDs is given twice.
pub fn objectid_block(block: u64) -> String {
    format!("{OBJECTIDS}{}", Padded(block))
}

/// cladefs: the prefix of `tenant`'s cells: listed by delimiter, its cells' prefixes
/// ([`crate::cell::of_tenant`]).
pub fn cells_of(tenant: u64) -> String {
    format!("{TENANTS}{}/cells/", Padded(tenant))
}

/// cladefs: the tenant a prefix of a delimiter listing of [`TENANTS`] names, `tenants/<tenant>/`.
pub fn tenant_of_prefix(prefix: &str) -> Option<u64> {
    number(prefix.strip_prefix(TENANTS)?.strip_suffix('/')?)
}

/// cladefs: `tenant`'s KEK object, under the bucket KMS (doc-16, The bucket KMS; TASK-411.27): its
/// KEK wrapped by a master key, created (create-only) on the tenant's first wrap, rewrapped by a
/// rotation of the master keys (conditional), and replaced by a tombstone when the tenant is
/// shredded (`bucket_cell::bucket_kms`). Under the tenant's prefix, beside its cells', no cell's:
/// no collector pass lists it, and none deletes it.
pub fn kek(tenant: u64) -> String {
    format!("{TENANTS}{}/{KEK}", Padded(tenant))
}

/// cladefs: what [`kek`] names under a tenant's prefix.
const KEK: &str = "kek";

/// cladefs: the cell whose prefix `key` is under, and the rest of it: none for a key of no cell's.
fn in_cell(key: &str) -> Option<(CellId, &str)> {
    let (tenant, rest) = key.strip_prefix(TENANTS)?.split_once('/')?;
    let (id, rest) = rest.strip_prefix("cells/")?.split_once('/')?;
    Some((CellId { tenant: number(tenant)?, id: number(id)? }, rest))
}

/// cladefs: the cell whose prefix `key` is under, a key or a prefix: none for one of no cell's.
pub fn cell_of(key: &str) -> Option<CellId> {
    in_cell(key).map(|(cell, _)| cell)
}

/// cladefs: the number of the bucket a request of `key`, a key or a prefix, goes to (TASK-633.4):
/// that of the bucket its cell's ID carries ([`crate::objectid::bucket_of`]), or, for a key of the
/// ephemeral cells' index, the bucket it names, its references' (board 8517, [`ephemeral_index`]);
/// none for a key of no cell, or of a cell tests name (number [`crate::identity::NAMED`]), whose
/// requests stay in the bucket asked. What a routed bucket routes by, and the simulation's buckets
/// of regions.
pub fn routed_to(key: &str) -> Option<u16> {
    let number = match ephemeral_under(key) {
        Some((bucket, _)) => bucket,
        None => crate::objectid::bucket_of(cell_of(key)?.id),
    };
    (number != crate::identity::NAMED).then_some(number)
}

/// cladefs: the cell whose whole prefix `prefix` is, as a listing of every object of the cell's
/// names it: none for any other prefix.
pub fn cell_of_prefix(prefix: &str) -> Option<CellId> {
    in_cell(prefix).filter(|(_, rest)| rest.is_empty()).map(|(cell, _)| cell)
}

/// cladefs: the longest key, in bytes, the buckets cells live in take (S3, GCS, Azure; a macOS file
/// bucket's PATH_MAX): a request of a longer one each refuses, for good (board 5861).
pub const MAX_KEY: usize = 1024;

/// cladefs: the collector's lease over `cell`, which its passes over the cell take (doc-7, Garbage
/// collection; TASK-395.2), a [`crate::passed::CollectorLease`]: passes over other
/// cells run beside them, unfenced.
pub fn collector_lease(cell: CellId) -> String {
    format!("{}{GC_LEASE}", cell.prefix())
}

/// cladefs: the prefix of the references of forks of `cell` ([`reference`]): listed by delimiter,
/// its distinct origins, which a pass over it keeps the states of ([`origin`]).
pub fn references(cell: CellId) -> String {
    format!("{}{REFS}", cell.prefix())
}

/// cladefs: the prefix of the references of the forks of `parent` at the end of its segment `at`:
/// listed, their children.
pub fn origin(parent: CellId, at: u64) -> String {
    format!("{}{}/", references(parent), Padded(at))
}

/// cladefs: the reference of the fork of `parent` at the end of its segment `at` to `child`, under
/// the parent's cell, the cell holding what the child inherits, whatever the child's (doc-7,
/// References; TASK-395.2): while it stands, the parent's cell's passes keep the parent's whole
/// state at the origin. Its bytes are a [`Reference`], unique to the fork, so that every writer
/// of it writes the same; the child's cell's passes retire it, computing it from the child's
/// record's origin, never listing it.
pub fn reference(parent: CellId, at: u64, child: CellId) -> String {
    format!("{}{}", origin(parent, at), child_name(child))
}

/// cladefs: the reference of an ephemeral cell, `child`, kept on the host of incarnation
/// `incarnation` alone, forked from `parent`'s version at the end of its segment `at` (doc-25,
/// TASK-698.3): among the version's children's references, keeping the parent's state at the
/// origin while it stands, as theirs do. No pass retires it, nor deletes it: no record of its
/// child's is ever in a bucket. Its host deletes it, its cell gone, or, the host gone for good,
/// its decommissioning ([`crate::lifecycle::decommission`]), by its index ([`ephemeral_index`]).
pub fn ephemeral_reference(parent: CellId, at: u64, incarnation: u64, child: CellId) -> String {
    format!("{}{EPHEMERAL}.{}.{}", origin(parent, at), Padded(incarnation), child_name(child))
}

/// cladefs: the number of the bucket an ephemeral cell forked from `parent` has its reference and
/// its index entry in: `parent`'s, the bucket the parent's ID carries.
pub fn ephemeral_bucket(parent: CellId) -> u16 {
    crate::objectid::bucket_of(parent.id)
}

/// cladefs: the prefix of the index of the ephemeral cells' references in the bucket numbered
/// `bucket`, every incarnation's, and of its incarnations' tombstones ([`decommissioned`]):
/// listed, every host's with ephemeral cells forked from cells of that bucket.
pub fn ephemeral_hosts(bucket: u16) -> String {
    format!("{EPHEMERAL}/{}/", Padded(u64::from(bucket)))
}

/// cladefs: the prefix of the index of the ephemeral cells' references in the bucket numbered
/// `bucket` whose host is of incarnation `incarnation` ([`ephemeral_index`]).
pub fn ephemeral_indexes(bucket: u16, incarnation: u64) -> String {
    format!("{}{}/", ephemeral_hosts(bucket), Padded(incarnation))
}

/// cladefs: the index entry of the reference of the ephemeral cell `child` of incarnation
/// `incarnation`'s host, forked from `parent`'s version at the end of its segment `at`
/// ([`ephemeral_reference`]): at the bucket's root, by the bucket of its reference
/// ([`ephemeral_bucket`]), then the incarnation, so that it is kept, and routed
/// ([`routed_to`]), in the bucket its reference is in, whichever host wrote it and whichever's
/// bucket it went through, and decommissioning the incarnation finds it listing that prefix in
/// each bucket (board 8517, hotbed question 479; [`crate::lifecycle::decommission`]). Written
/// before the reference, and deleted after it.
pub fn ephemeral_index(incarnation: u64, parent: CellId, at: u64, child: CellId) -> String {
    format!(
        "{}{}.{}.{}",
        ephemeral_indexes(ephemeral_bucket(parent), incarnation),
        child_name(parent),
        Padded(at),
        child_name(child)
    )
}

/// cladefs: the tombstone of incarnation `incarnation` in the bucket numbered `bucket` (board
/// 8519): there once its decommissioning began, for good, in every bucket. A host's reference of
/// that incarnation written in that bucket is taken back and refused once it is seen there
/// ([`crate::lifecycle::refer_ephemeral`]); beside its index entries' prefix, never under it, so
/// that listing them lists not it.
pub fn decommissioned(bucket: u16, incarnation: u64) -> String {
    format!("{}{}.{DECOMMISSIONED}", ephemeral_hosts(bucket), Padded(incarnation))
}

/// cladefs: of a key or a prefix under the ephemeral cells' index, the bucket it names and the
/// rest after it: none for any other. The bucket's number written as [`ephemeral_hosts`] writes
/// it, fixed-width base 32 ([`Padded`]), alone.
fn ephemeral_under(key: &str) -> Option<(u16, &str)> {
    let rest = key.strip_prefix(EPHEMERAL)?.strip_prefix('/')?;
    let (bucket, rest) = rest.split_once('/')?;
    // Not a bucket's number: no key of the index.
    let number = u16::try_from(number(bucket)?).ok()?;
    Some((number, rest))
}

/// cladefs: the parent, origin and child an ephemeral cell's index entry names, and its
/// incarnation, by its key ([`ephemeral_index`]), if it is one: in the bucket of its parent's.
pub fn ephemeral_indexed(key: &str) -> Option<(u64, CellId, u64, CellId)> {
    let (bucket, rest) = ephemeral_under(key)?;
    let (incarnation, rest) = rest.split_once('/')?;
    let words: Vec<&str> = rest.split('.').collect();
    let [parent_tenant, parent, at, tenant, child] = words[..] else { return None };
    let parent = CellId { tenant: number(parent_tenant)?, id: number(parent)? };
    let child = CellId { tenant: number(tenant)?, id: number(child)? };
    let indexed = (number(incarnation)?, parent, number(at)?, child);
    (ephemeral_bucket(parent) == bucket).then_some(indexed)
}

/// cladefs: the bucket and incarnation an incarnation's tombstone names, by its key
/// ([`decommissioned`]), if it is one.
pub fn decommissioned_of(key: &str) -> Option<(u16, u64)> {
    let (bucket, rest) = ephemeral_under(key)?;
    let incarnation = rest.strip_suffix(DECOMMISSIONED)?.strip_suffix('.')?;
    Some((bucket, number(incarnation)?))
}

/// cladefs: the legacy v31/v32 single-key mirror pin of `parent`'s version at `at` in another
/// bucket (`refs/<at>/mirror`), retained for backwards-compatible decoding and GC cleanup.
pub fn legacy_pin(parent: CellId, at: u64) -> String {
    format!("{}{PIN}", origin(parent, at))
}

/// cladefs: the `/`-terminated prefix of every v33 per-run mirror pin of `parent`'s version at
/// the end of its segment `at` in another bucket (`refs/<at>/mirror-pins/`, board 11699).
pub fn pins(parent: CellId, at: u64) -> String {
    format!("{}{MIRROR_PINS}", origin(parent, at))
}

/// cladefs: the write-once pin of mirror run `run` for `parent`'s version at the end of its
/// segment `at` in another bucket (doc-23, Mirrors; TASK-633.8; board 11699): a reference of the
/// mirror run's own, among those of the version's children there, which keeps the copy from that
/// bucket's collector while it is made, as a child's reference keeps what it inherits. Disjoint per
/// run so plain unconditional DELETE letting go never removes another run's pin.
pub fn pin(parent: CellId, at: u64, run: u64) -> String {
    format!("{}{}", pins(parent, at), Padded(run))
}

/// cladefs: the legacy v31/v32 single-key mirror hold of `parent`'s version at `at` for `bucket`
/// in the parent's bucket (`refs/<at>/mirror.<B>`), retained for backwards-compatible decoding and
/// GC cleanup.
pub fn legacy_hold(parent: CellId, at: u64, bucket: u16) -> String {
    format!("{}{PIN}.{}", origin(parent, at), Padded(u64::from(bucket)))
}

/// cladefs: the `/`-terminated prefix of every v33 per-run mirror hold of `parent`'s version at
/// the end of its segment `at` for the bucket numbered `bucket`, in the parent's bucket
/// (`refs/<at>/mirror-holds/<B>/`, board 11699).
pub fn holds(parent: CellId, at: u64, bucket: u16) -> String {
    format!("{}{MIRROR_HOLDS}{}/", origin(parent, at), Padded(u64::from(bucket)))
}

/// cladefs: the write-once hold of mirror run `run` for `parent`'s version at the end of its
/// segment `at` in the bucket numbered `bucket`, in the parent's bucket (doc-23, Mirrors;
/// TASK-633.8; board 11699): a reference of the mirror run's own, among those of the version's
/// children there, which keeps the version's state there whole, and the parent's cell from being
/// removed, with what the version inherits from other cells, while the copy stands. Disjoint per
/// run so plain unconditional DELETE letting go never removes another run's hold; the mirror's
/// bucket's collector lists [`holds`] (and checks [`legacy_hold`]) and deletes all holds for
/// `bucket` as the copy goes.
pub fn hold(parent: CellId, at: u64, bucket: u16, run: u64) -> String {
    format!("{}{}", holds(parent, at, bucket), Padded(run))
}

/// cladefs: the hold key in the parent's bucket corresponding to pin key `key` in `bucket`
/// (board 11699): maps a v33 per-run pin (`refs/<at>/mirror-pins/<run>`) to its matching per-run
/// hold (`refs/<at>/mirror-holds/<B>/<run>`) and a legacy v31/v32 pin (`refs/<at>/mirror`) to its
/// legacy hold (`refs/<at>/mirror.<B>`). None if `key` is not a pin.
pub fn hold_of_pin(key: &str, bucket: u16) -> Option<String> {
    let (cell, rest) = in_cell(key)?;
    let rest = rest.strip_prefix(REFS)?;
    let (at, name) = rest.split_once('/')?;
    let at = number(at)?;
    if name == PIN {
        return Some(legacy_hold(cell, at, bucket));
    }
    let run = name.strip_prefix(MIRROR_PINS)?;
    Some(hold(cell, at, bucket, number(run)?))
}

/// cladefs: the marker of the mirror of `parent`'s version at the end of its segment `at` in
/// another bucket (doc-23, Mirrors; TASK-633.8): created once every object of the version is
/// copied there, never before; the switch of the version's reads, and of its children's
/// references, to that bucket ([`Mirror`]).
pub fn mirror(parent: CellId, at: u64) -> String {
    format!("{}{MIRRORS}{}", parent.prefix(), Padded(at))
}

/// cladefs: the prefix of the read evidence of `cell`'s versions ([`evidence`]).
pub fn evidence_of_cell(cell: CellId) -> String {
    format!("{}{READS}", cell.prefix())
}

/// cladefs: the prefix of the read evidence of `parent`'s version at the end of its segment `at`
/// that `tenant`'s forks of it wrote ([`evidence`]): what those forks read of it.
pub fn evidence_of(parent: CellId, at: u64, tenant: u64) -> String {
    format!("{}{}/{}/", evidence_of_cell(parent), Padded(at), Padded(tenant))
}

/// cladefs: a recording of what a fork of `parent`'s version at the end of its segment `at`, a
/// cell of `tenant`'s, read of it as it opened ([`crate::evidence`], TASK-702.2): created once, by
/// the fork's host, at an ID it drew, under the version's cell, so that the pass over that cell
/// deletes it with the version, or once newer evidence of the tenant's replaces it.
pub fn evidence(parent: CellId, at: u64, tenant: u64, id: u64) -> String {
    format!("{}{}", evidence_of(parent, at, tenant), Padded(id))
}

/// cladefs: the prefix of the markers of `parent`'s versions mirrored in a bucket ([`mirror`]).
pub fn mirrors(parent: CellId) -> String {
    format!("{}{MIRRORS}", parent.prefix())
}

/// cladefs: `child` as the last part of a reference's key, or its mark's: `TENANT.CELL`.
fn child_name(child: CellId) -> String {
    format!("{}.{}", Padded(child.tenant), Padded(child.id))
}

/// cladefs: the mark of the reference of the fork of `parent` at the end of its segment `at` to
/// `child` ([`reference`]), retired or taken back, under the parent's cell beside the references,
/// outside their prefix, which a pass lists by delimiter (TASK-395.13, TASK-395.14): written before
/// the reference is deleted, and kept for good, its bytes a [`Retired`]. The reference is garbage
/// from then on, no record ever to name its fork: one a writer whose read of the child's record
/// came before writes again after, the pass over the parent's cell that lists it beside its mark
/// deletes. Nothing else would look at it again.
pub fn retired(parent: CellId, at: u64, child: CellId) -> String {
    format!("{}{RETIRED}{}/{}", parent.prefix(), Padded(at), child_name(child))
}

/// cladefs: the mark of the reference at `key` ([`retired`]), if it is a reference's key.
pub fn retired_of(key: &str) -> Option<String> {
    match kind(key) {
        Kind::Reference { parent, at, child } => Some(retired(parent, at, child)),
        _ => None,
    }
}

/// cladefs: the origin a prefix of `parent`'s references names, `<C>refs/<at>/`, as a delimiter
/// listing of [`references`] gives it.
pub fn referenced_at(parent: CellId, prefix: &str) -> Option<u64> {
    number(prefix.strip_prefix(&references(parent))?.strip_suffix('/')?)
}

/// What kind of object a key names, and what its key says. cladefs: an extent's says the segment
/// that names it, `segment`, of the writer of `epoch`, which uploaded it before: what it may yet be
/// named by, as its writer declares it (TASK-79.3, TASK-153.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Segment {
        cell: CellId,
        segment: u64,
        epoch: u64,
    },
    Extent {
        cell: CellId,
        epoch: u64,
        segment: u64,
        sequence: u64,
    },
    Checkpoint {
        cell: CellId,
        segment: u64,
        epoch: u64,
    },
    /// cladefs: a cell's head ([`head`]).
    Head {
        cell: CellId,
    },
    /// cladefs: a block of object IDs taken ([`objectid_block`]).
    ObjectIds {
        block: u64,
    },
    /// cladefs: the bucket's identity ([`IDENTITY`]): kept for good.
    Identity,
    /// cladefs: the record of every bucket ([`BUCKETS`]): kept for good.
    Buckets,
    /// cladefs: a tenant's KEK object under the bucket KMS ([`kek`], TASK-411.27): no cell's, kept
    /// for good.
    Kek {
        tenant: u64,
    },
    /// cladefs: the collector's lease over a cell ([`gc_lease`]).
    CollectorLease {
        cell: CellId,
    },
    /// cladefs: a key of the cell's of a format before this one that nothing of this one makes or
    /// reads (doc-27): the slots of a head kept in many objects (28's), of `format`, the newest
    /// that wrote such a key. Refused ([`crate::Error::Format`]) where found.
    Former {
        cell: CellId,
        format: u32,
    },
    /// cladefs: the reference of the fork of `parent`, of the cell it is under, at the end of its
    /// segment `at`, to `child` ([`reference`]): never a candidate.
    Reference {
        parent: CellId,
        at: u64,
        child: CellId,
    },
    /// cladefs: the mark of that reference, retired or taken back ([`retired`]): never a candidate.
    Retired {
        parent: CellId,
        at: u64,
        child: CellId,
    },
    /// cladefs: the pin of the mirror of `parent`'s version at `at` in another bucket ([`pin`]): a
    /// reference of the mirror's own, never a candidate.
    Pin {
        parent: CellId,
        at: u64,
    },
    /// cladefs: the hold of the mirror of `parent`'s version at `at` in the bucket numbered
    /// `bucket`, in the parent's bucket ([`hold`]): a reference of the mirror's own, never a
    /// candidate.
    Hold {
        parent: CellId,
        at: u64,
        bucket: u16,
    },
    /// cladefs: the marker of the mirror of `cell`'s version at `at` in another bucket
    /// ([`mirror`]).
    Mirror {
        cell: CellId,
        at: u64,
    },
    /// cladefs: an ephemeral cell's reference ([`ephemeral_reference`], doc-25): never a candidate,
    /// its host's to delete.
    Ephemeral {
        parent: CellId,
        at: u64,
        incarnation: u64,
        child: CellId,
    },
    /// cladefs: the index entry of an ephemeral cell's reference ([`ephemeral_index`]): no cell's,
    /// never a candidate, its host's or its decommissioning's to delete.
    EphemeralIndex {
        incarnation: u64,
    },
    /// cladefs: the tombstone of a host's incarnation decommissioned ([`decommissioned`], board
    /// 8519): no cell's, never a candidate, kept for good.
    Decommissioned {
        incarnation: u64,
    },
    /// cladefs: a recording of what a fork of `parent`'s version at `at`, of `tenant`'s, read of
    /// it ([`evidence`], TASK-702.2): no pass's candidate, nor in its listing's digest; the pass
    /// over `parent` trims it ([`crate::evidence::doomed`]).
    Evidence {
        parent: CellId,
        at: u64,
        tenant: u64,
    },
    /// cladefs: a fast WAL segment in an appendable zonal bucket ([`wal`], doc-29): never deleted
    /// by the cold-bucket collector (managed by fast-WAL rollover and epoch sweep using 36-byte
    /// authenticated tombstones).
    Wal {
        cell: CellId,
        epoch: u64,
        segment: u64,
    },
    Unknown,
}

impl Kind {
    /// cladefs: its name, as an operator reads it (`cladefs-gc --dry-run`).
    pub fn name(&self) -> &'static str {
        match self {
            Self::Segment { .. } => "segment",
            Self::Extent { .. } => "extent",
            Self::Checkpoint { .. } => "checkpoint",
            Self::Head { .. } => "head",
            Self::ObjectIds { .. } => "objectid",
            Self::Identity => "identity",
            Self::Buckets => "buckets",
            Self::Kek { .. } => "kek",
            Self::CollectorLease { .. } => "gc-lease",
            Self::Former { .. } => "former",
            Self::Reference { .. } => "reference",
            Self::Retired { .. } => "retired",
            Self::Pin { .. } => "pin",
            Self::Hold { .. } => "hold",
            Self::Mirror { .. } => "mirror",
            Self::Ephemeral { .. } => "ephemeral",
            Self::EphemeralIndex { .. } => "ephemeral-index",
            Self::Decommissioned { .. } => "decommissioned",
            Self::Evidence { .. } => "evidence",
            Self::Wal { .. } => "wal",
            Self::Unknown => "unknown",
        }
    }
}

/// cladefs: the cell whose object `key` is, if a cell's: every kind under its prefix, the moves of
/// its objects and of their moves, its collector's lease, and the references of forks of
/// its cells, under the parent's cell (`cladefs-gc --usage`, TASK-394, TASK-395.2).
pub fn owner(key: &str) -> Option<CellId> {
    match kind(key) {
        Kind::Segment { cell, .. }
        | Kind::Extent { cell, .. }
        | Kind::Checkpoint { cell, .. }
        | Kind::Head { cell, .. }
        | Kind::Wal { cell, .. }
        | Kind::Reference { parent: cell, .. }
        | Kind::Retired { parent: cell, .. }
        | Kind::Pin { parent: cell, .. }
        | Kind::Hold { parent: cell, .. }
        | Kind::Ephemeral { parent: cell, .. }
        | Kind::Evidence { parent: cell, .. }
        | Kind::Mirror { cell, .. } => Some(cell),
        Kind::CollectorLease { cell } => Some(cell),
        Kind::Former { cell, .. } => Some(cell),
        Kind::ObjectIds { .. }
        | Kind::EphemeralIndex { .. }
        | Kind::Decommissioned { .. }
        | Kind::Identity
        | Kind::Buckets
        | Kind::Kek { .. }
        | Kind::Unknown => None,
    }
}

/// The number a key's part writes ([`Padded`]).
fn number(text: &str) -> Option<u64> {
    base32_id::parse_padded(text)
}

pub fn kind(key: &str) -> Kind {
    let parsed = || -> Option<Kind> {
        if key == IDENTITY {
            return Some(Kind::Identity);
        }
        if key == BUCKETS {
            return Some(Kind::Buckets);
        }
        if let Some(block) = key.strip_prefix(OBJECTIDS) {
            return Some(Kind::ObjectIds { block: number(block)? });
        }
        if let Some((incarnation, ..)) = ephemeral_indexed(key) {
            return Some(Kind::EphemeralIndex { incarnation });
        }
        if let Some((_, incarnation)) = decommissioned_of(key) {
            return Some(Kind::Decommissioned { incarnation });
        }
        if let Some(tenant) =
            (key.strip_prefix(TENANTS)).and_then(|rest| rest.strip_suffix(KEK)?.strip_suffix('/'))
        {
            return Some(Kind::Kek { tenant: number(tenant)? });
        }
        let (cell, rest) = in_cell(key)?;
        if rest.starts_with(HEAD_SLOTS) {
            return Some(Kind::Former { cell, format: 28 });
        }
        if let Some(rest) = rest.strip_prefix(GC) {
            if rest == GC_LEASE.strip_prefix(GC)? {
                return Some(Kind::CollectorLease { cell });
            }
            // cladefs: no other: the marks of mark-only passes went (board 10545).
            return None;
        }
        // A reference's key, or its mark's, the rest after their prefixes alike.
        let fork = |rest: &str| -> Option<(CellId, u64, CellId)> {
            let (at, child) = rest.split_once('/')?;
            let (tenant, child) = child.split_once('.')?;
            let child = CellId { tenant: number(tenant)?, id: number(child)? };
            Some((cell, number(at)?, child))
        };
        if let Some(rest) = rest.strip_prefix(REFS) {
            if let Some((at, name)) = rest.split_once('/') {
                if name == PIN {
                    return Some(Kind::Pin { parent: cell, at: number(at)? });
                }
                if let Some(run) = name.strip_prefix(MIRROR_PINS) {
                    number(run)?;
                    return Some(Kind::Pin { parent: cell, at: number(at)? });
                }
                if let Some(rest) = name.strip_prefix(MIRROR_HOLDS) {
                    let (bucket, run) = rest.split_once('/')?;
                    number(run)?;
                    // Not a bucket's number: no hold's key.
                    let bucket = u16::try_from(number(bucket)?).ok()?;
                    return Some(Kind::Hold { parent: cell, at: number(at)?, bucket });
                }
                if let Some(bucket) = name.strip_prefix(PIN).and_then(|rest| rest.strip_prefix('.'))
                {
                    // Legacy v31/v32 single-key hold (`refs/<at>/mirror.<B>`).
                    let bucket = u16::try_from(number(bucket)?).ok()?;
                    return Some(Kind::Hold { parent: cell, at: number(at)?, bucket });
                }
                if let Some(rest) = name.strip_prefix(EPHEMERAL).and_then(|r| r.strip_prefix('.')) {
                    let (incarnation, child) = rest.split_once('.')?;
                    let (tenant, child) = child.split_once('.')?;
                    let child = CellId { tenant: number(tenant)?, id: number(child)? };
                    let (at, incarnation) = (number(at)?, number(incarnation)?);
                    return Some(Kind::Ephemeral { parent: cell, at, incarnation, child });
                }
            }
            let (parent, at, child) = fork(rest)?;
            return Some(Kind::Reference { parent, at, child });
        }
        if let Some(at) = rest.strip_prefix(MIRRORS) {
            return Some(Kind::Mirror { cell, at: number(at)? });
        }
        if let Some(rest) = rest.strip_prefix(READS) {
            let [at, tenant, id] = rest.split('/').collect::<Vec<_>>()[..] else { return None };
            number(id)?;
            return Some(Kind::Evidence { parent: cell, at: number(at)?, tenant: number(tenant)? });
        }
        if let Some(rest) = rest.strip_prefix(RETIRED) {
            let (parent, at, child) = fork(rest)?;
            return Some(Kind::Retired { parent, at, child });
        }
        if rest == "head" {
            return Some(Kind::Head { cell });
        }
        // A segment's or a checkpoint's key: its writer's epoch, then its number.
        let numbered = |rest: &str| -> Option<(u64, u64)> {
            let (epoch, segment) = rest.split_once('.')?;
            Some((number(segment)?, number(epoch)?))
        };
        if let Some(rest) = rest.strip_prefix("segments/") {
            let (segment, epoch) = numbered(rest)?;
            return Some(Kind::Segment { cell, segment, epoch });
        }
        if let Some(rest) = rest.strip_prefix("checkpoints/") {
            let (segment, epoch) = numbered(rest)?;
            return Some(Kind::Checkpoint { cell, segment, epoch });
        }
        if let Some(rest) = rest.strip_prefix("extents/") {
            let [epoch, segment, sequence] = rest.split('.').collect::<Vec<_>>()[..] else {
                return None;
            };
            let (epoch, segment, sequence) = (number(epoch)?, number(segment)?, number(sequence)?);
            return Some(Kind::Extent { cell, epoch, segment, sequence });
        }
        if let Some(rest) = rest.strip_prefix(WAL) {
            let (epoch, segment) = rest.split_once('/')?;
            return Some(Kind::Wal { cell, epoch: number(epoch)?, segment: number(segment)? });
        }
        None
    };
    parsed().unwrap_or(Kind::Unknown)
}

/// A segment: its data, then its footer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub data: Vec<u8>,
    pub footer: Footer,
}

/// cladefs: the cell of tests' cells, of tenant 1.
#[cfg(any(test, feature = "testing"))]
pub const TEST_CELL: CellId = CellId { tenant: 1, id: 1 };

/// cladefs: tests' cell `id`, of [`TEST_CELL`]'s tenant.
#[cfg(any(test, feature = "testing"))]
pub const fn test_cell(id: u64) -> CellId {
    CellId { tenant: TEST_CELL.tenant, id }
}

/// cladefs: the default tenant (TASK-364), of the cells made for no tenant: public bases, images,
/// tests'. Its versions fork into any tenant's cells, shared or not ([`Version::shared`]).
pub const DEFAULT_TENANT: u64 = 0;

/// Where a cell was forked from: ordered by its parent, then its segment, so that the forks made
/// at one segment sort together. Carries the writer `epoch` of segment `at` so cross-cell recovery
/// reads `checkpoint(parent, at, epoch)` directly without reading `parent`'s head (board 6017).
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    TypeFingerprint,
)]
pub struct Origin {
    pub parent: CellId,
    pub at: u64,
    pub epoch: u64,
}

/// A cell's head (TASK-407, TASK-515), its one swapped object: what made it and whose it is
/// ([`crate::record`]), its keys ([`crate::keys`]), its kind ([`crate::cell`]), its writer's
/// taking, of the epoch after the taking before, by `holder`, with the fencing token `token`; and
/// what the cell holds, its segments to `end`, its newest checkpoint and its versions. Swapped with
/// a condition (S3 If-Match, GCS generation), by its writer alone while it holds it: a segment or a
/// checkpoint is the cell's only once the head names it, so that one its writer uploaded, taken
/// over before it swapped the head, never is, nor ever will be, and the collector deletes it. A
/// takeover swaps it to the next epoch, which fences the writer before: its next swap fails. Epoch
/// 0 is a head made and never taken, a fork's child's until its fork is made; [`Head::FINAL`] a
/// deleted cell's: no writer takes it again. Heads are never deleted, but with a dead cell
/// ([`Head::removable`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub struct Head {
    /// The format of the cell's objects ([`FORMAT`]), first, so that a reader of any format reads
    /// it before the rest, which it reads only if it is of its own ([`Head::decode`], TASK-141.2,
    /// TASK-165.3).
    pub format: u32,
    /// The cell's owner: a number its maker chose at random, unique to that making, which every
    /// head of the cell names, whoever writes it ([`Child::owner`]). So no two makers of one cell
    /// ID write the same bytes, and a create that finds a head there tells its own, landed though
    /// its answer was lost, from another's (TASK-79.82, TASK-153.9).
    pub owner: u64,
    /// What made the cell ([`crate::record::Made`]).
    pub made: crate::record::Made,
    /// cladefs: what the cell holds, which says what opens it, given by its first writer (doc-20).
    pub kind: Option<crate::cell::Kind>,
    /// cladefs: the cell's KEK, every generation of it, each wrapped by its tenant's KEK in the
    /// KMS (doc-16, TASK-411.3): every cell is made with one, none in the clear (TASK-411.28).
    pub keks: Vec<crate::keys::Kek>,
    /// cladefs: whether the cell was shredded, its KEKs taken out ([`crate::keys::shred`],
    /// TASK-411.7): no one takes it again.
    pub shredded: bool,
    /// cladefs: the cell's DEK, each generation of it, wrapped by its KEK, oldest first: given by
    /// its first writer's taking ([`crate::keys::taking`]); a writer's ring of a head with none is
    /// refused (TASK-411.28).
    pub deks: Vec<crate::keys::Dek>,
    /// cladefs: the DEKs of every ancestor of another cell whose objects the cell reads, each
    /// generation, rewrapped under its own KEK (doc-16, Forks and shared versions; TASK-411.4),
    /// given by its fork's host as it makes the fork, so that its readers need no other cell's
    /// KEK, nor another tenant's KMS.
    pub inherited: Vec<crate::keys::InheritedDek>,
    pub epoch: u64,
    /// Who took it: unique to each taking, so that a write of it is recognised.
    pub holder: u64,
    /// The fencing token it was taken with, its writer's placement's, which no lower one takes the
    /// head from (TASK-39.7); `u64::MAX` once deleted.
    pub token: u64,
    /// The number of segments the cell holds: 0 to `end`, without gaps.
    pub end: u64,
    /// cladefs: the swaps of the head before this one ([`crate::head`]): one more each swap, so
    /// that every write of the head writes bytes of its own. A backend that conditions a write on
    /// an ETag of the object's bytes (S3's MD5) then never takes a delayed write, conditional on a
    /// version the head held, for one it holds again: a share then an unshare would bring back
    /// the bytes, and the ETag, of before.
    pub swaps: u64,
    /// The epochs of the writers of those segments, in runs: from each run's first number on, to
    /// the next run's, the segments are of its epoch. A run a takeover, at most.
    pub runs: Vec<(u64, u64)>,
    /// The cell's newest checkpoint, by its number and its writer's epoch, if it has one.
    pub checkpoint: Option<(u64, u64)>,
    /// cladefs: the cell's versions, oldest first (doc-18, TASK-476): each a name for its state
    /// as of a segment it names, kept for as long as it names it. The acknowledgement naming a
    /// segment names the versions cut at its end, in the same swap; a takeover names what the
    /// head did; its writer drops one by a swap; a deletion's final head names none.
    pub versions: Vec<Version>,
    /// cladefs: whether the cell is replicated, and how (doc-21, TASK-537): its commits on f+1 of
    /// 2f+1 hosts' disks before the bucket. None, as every cell was before, for a cell whose
    /// commits are durable off their host once uploaded alone. Given by a taking
    /// ([`crate::head::Taking::replication`]); its configuration swapped by its writer.
    pub replication: Option<Replication>,
    /// cladefs: whether the cell commits to an appendable fast WAL bucket (doc-29): S3 Express One
    /// Zone or GCS Rapid Storage, keeping compute nodes stateless cache-only. Mutually exclusive
    /// with active peer replication (`f >= 1`). `active` stays pointing to the previous epoch's
    /// fast WAL across a takeover until the taker fences, recovers, flushes recovered commits to
    /// cold segments, and switches `active` in the same head CAS.
    pub fast_wal: Option<FastWal>,
}

/// cladefs: the currently active fast WAL epoch and bucket URL recorded in [`Head::fast_wal`]
/// (doc-29). Keeping `bucket` in the head lets a taker in another availability zone fence and
/// recover the previous writer's zonal fast WAL before switching to its own local zonal bucket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub struct ActiveWal {
    pub epoch: u64,
    pub bucket: String,
}

/// cladefs: a cell's fast WAL configuration in [`Head::fast_wal`] (doc-29).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub struct FastWal {
    /// What a guest's `fsync` waits for on this cell: [`Fsync::Ordered`] or [`Fsync::FastWal`].
    pub fsync: Fsync,
    /// The active fast WAL epoch and bucket URL that may contain commits not yet acknowledged in
    /// the cold bucket head. None until the writer activates its WAL (or after recovery completes
    /// when disabling fast WAL).
    pub active: Option<ActiveWal>,
}

/// cladefs: what a guest's fsync waits for in a cell (doc-12, doc-21, doc-29): its commit's level.
/// The sync family (sync, syncfs, unmount) waits for [`Fsync::Replicated`] in a replicated cell,
/// [`Fsync::FastWal`] in a fast-WAL cell, and for its upload in any other.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub enum Fsync {
    /// In the daemon's memory, in order: survives what leaves the daemon alive.
    #[default]
    Ordered,
    /// On f+1 members' disks (or uploaded): survives the writer's host.
    Replicated,
    /// Appended to the cell's fast WAL in appendable zonal object storage (or uploaded): survives
    /// the writer's host with stateless cache-only nodes (doc-29).
    FastWal,
}

/// Frozen format 31 head types, kept for decoding existing version 31 heads (doc-28).
pub(crate) mod v31 {
    use super::{CellId, Configuration, Version};
    use fprint::TypeFingerprint;
    use serde::{Deserialize, Serialize};

    #[derive(
        Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint,
    )]
    pub(crate) enum Fsync {
        #[default]
        Ordered,
        Local,
        Replicated,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
    pub(crate) struct Wish {
        pub(crate) f: u8,
        pub(crate) fsync: Fsync,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
    pub(crate) struct Replication {
        pub(crate) wish: Wish,
        pub(crate) configuration: Option<Configuration>,
    }

    #[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
    pub(crate) struct Head {
        pub(crate) format: u32,
        pub(crate) owner: u64,
        pub(crate) made: crate::record::Made,
        pub(crate) kind: Option<crate::cell::Kind>,
        pub(crate) keks: Vec<crate::keys::Kek>,
        pub(crate) shredded: bool,
        pub(crate) deks: Vec<crate::keys::Dek>,
        pub(crate) inherited: Vec<crate::keys::InheritedDek>,
        pub(crate) epoch: u64,
        pub(crate) holder: u64,
        pub(crate) token: u64,
        pub(crate) end: u64,
        pub(crate) swaps: u64,
        pub(crate) runs: Vec<(u64, u64)>,
        pub(crate) checkpoint: Option<(u64, u64)>,
        pub(crate) versions: Vec<Version>,
        pub(crate) replication: Option<Replication>,
    }

    impl Head {
        pub(crate) fn into_current(self, cell: CellId) -> super::Head {
            let replication = self.replication.map(|replication| {
                let fsync = match replication.wish.fsync {
                    Fsync::Ordered => super::Fsync::Ordered,
                    Fsync::Local => {
                        log::warn!(
                            "cladefs: cell {cell}'s format 31 head uses removed Fsync::Local; \
                             treating as Fsync::Ordered"
                        );
                        super::Fsync::Ordered
                    }
                    Fsync::Replicated => super::Fsync::Replicated,
                };
                super::Replication {
                    wish: super::Wish { f: replication.wish.f, fsync },
                    configuration: replication.configuration,
                }
            });
            super::Head {
                format: super::FORMAT.current,
                owner: self.owner,
                made: self.made,
                kind: self.kind,
                keks: self.keks,
                shredded: self.shredded,
                deks: self.deks,
                inherited: self.inherited,
                epoch: self.epoch,
                holder: self.holder,
                token: self.token,
                end: self.end,
                swaps: self.swaps,
                runs: self.runs,
                checkpoint: self.checkpoint,
                versions: self.versions,
                replication,
                fast_wal: None,
            }
        }
    }
}

/// Frozen format 32 head types, kept for decoding existing version 32 heads (doc-28, doc-29).
pub(crate) mod v32 {
    use super::{Configuration, Version};
    use fprint::TypeFingerprint;
    use serde::{Deserialize, Serialize};

    #[derive(
        Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint,
    )]
    pub(crate) enum Fsync {
        #[default]
        Ordered,
        Replicated,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
    pub(crate) struct Wish {
        pub(crate) f: u8,
        pub(crate) fsync: Fsync,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
    pub(crate) struct Replication {
        pub(crate) wish: Wish,
        pub(crate) configuration: Option<Configuration>,
    }

    #[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
    pub(crate) struct Head {
        pub(crate) format: u32,
        pub(crate) owner: u64,
        pub(crate) made: crate::record::Made,
        pub(crate) kind: Option<crate::cell::Kind>,
        pub(crate) keks: Vec<crate::keys::Kek>,
        pub(crate) shredded: bool,
        pub(crate) deks: Vec<crate::keys::Dek>,
        pub(crate) inherited: Vec<crate::keys::InheritedDek>,
        pub(crate) epoch: u64,
        pub(crate) holder: u64,
        pub(crate) token: u64,
        pub(crate) end: u64,
        pub(crate) swaps: u64,
        pub(crate) runs: Vec<(u64, u64)>,
        pub(crate) checkpoint: Option<(u64, u64)>,
        pub(crate) versions: Vec<Version>,
        pub(crate) replication: Option<Replication>,
    }

    impl Head {
        pub(crate) fn into_current(self) -> super::Head {
            let replication = self.replication.map(|replication| {
                let fsync = match replication.wish.fsync {
                    Fsync::Ordered => super::Fsync::Ordered,
                    Fsync::Replicated => super::Fsync::Replicated,
                };
                super::Replication {
                    wish: super::Wish { f: replication.wish.f, fsync },
                    configuration: replication.configuration,
                }
            });
            super::Head {
                format: super::FORMAT.current,
                owner: self.owner,
                made: self.made,
                kind: self.kind,
                keks: self.keks,
                shredded: self.shredded,
                deks: self.deks,
                inherited: self.inherited,
                epoch: self.epoch,
                holder: self.holder,
                token: self.token,
                end: self.end,
                swaps: self.swaps,
                runs: self.runs,
                checkpoint: self.checkpoint,
                versions: self.versions,
                replication,
                fast_wal: None,
            }
        }
    }
}

/// cladefs: the most f a cell is replicated with: 2f+1 = 7 copies.
pub const MAX_F: u8 = 3;

/// cladefs: what a replicated cell asks for, as its taking gives it ([`Head::replication`]): its
/// f, and what its guest's fsync waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub struct Wish {
    pub f: u8,
    pub fsync: Fsync,
}

impl Wish {
    /// A wish of `f` and `fsync`, unless `f` is past [`MAX_F`], `fsync` is [`Fsync::FastWal`]
    /// (which belongs on [`Head::fast_wal`], not replication), or `fsync` waits for copies an f of
    /// 0 never makes.
    pub fn new(f: u8, fsync: Fsync) -> Option<Self> {
        (f <= MAX_F && fsync != Fsync::FastWal && !(f == 0 && fsync == Fsync::Replicated))
            .then_some(Self { f, fsync })
    }
}

/// cladefs: a replicated cell's replication (doc-21): what it asks for, and the configuration
/// its writer's commits are replicated in, once it has one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub struct Replication {
    pub wish: Wish,
    /// The active configuration: none until a writer installs one.
    pub configuration: Option<Configuration>,
}

/// cladefs: a configuration of a replicated cell (doc-21): its writer and 2f members, holding a
/// write-once log of the commits the head does not name yet. Made by a taking, or by its writer
/// replacing members, and active once the head names it. An f of 0, its writer alone, holds no
/// quorum: a taker of a cell whose head names it reads the bucket alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub struct Configuration {
    /// Its number, greater than every configuration's before it in the cell.
    pub number: u64,
    /// The epoch of the writer that made it, the only one that appends to its log.
    pub epoch: u64,
    pub f: u8,
    /// Its members, its writer first.
    pub members: Vec<Member>,
}

/// cladefs: a member of a configuration (doc-21): a host, by its peer address, in one incarnation
/// of its replica directory, which a host that lost its disk, and the promises on it, is no more.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, TypeFingerprint)]
pub struct Member {
    /// `HOST:PORT`, as the hosts sharing the bucket name it (`CLADEFSD_PEERS`).
    pub address: String,
    pub incarnation: u64,
}

/// cladefs: a version a head names ([`Head::versions`], doc-18, doc-20): its ID, which the footer
/// of the segment making it named ([`NewVersion`]); the segment it is at and that segment's writer
/// `epoch` (board 6017); and whether forks of it may be of other tenants' cells (doc-7, Shared
/// versions).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub struct Version {
    pub id: u64,
    pub at: u64,
    pub epoch: u64,
    pub shared: bool,
}

/// cladefs: the most versions a head names (doc-18): an acknowledgement writes them all, 25 bytes
/// each, so that a head stays a few KB. A cut past it is refused, unless its window drops the
/// oldest.
pub const MAX_VERSIONS: usize = 256;

impl Head {
    pub const FINAL: u64 = u64::MAX;

    /// A head of `cell` made by `made`, of `owner`, of this format, never taken.
    pub fn of(made: crate::record::Made, owner: u64) -> Self {
        Self { format: FORMAT.current, owner, made, ..Self::default() }
    }

    /// The head of `cell` that `bytes` encode, unless it is of a format version this build doesn't
    /// read, which its first bytes say ([`crate::Error::Format`]). cladefs: refused as corrupt
    /// unless its runs name every segment of `0..end` (board 8545): a bucket's object, untrusted
    /// until a MAC is checked, which the keyless collector checks none of; past here, the epoch of
    /// each segment it names is there ([`Head::segments`]).
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    pub fn decode(cell: CellId, bytes: &[u8]) -> Result<Self, crate::Error> {
        let format: u32 = decode(bytes.get(..4).unwrap_or(bytes))?;
        let format = FORMAT.check(format).map_err(|_| crate::Error::Format { cell, format })?;
        let head: Self = match format {
            31 => decode::<v31::Head>(bytes)?.into_current(cell),
            32..=34 => decode::<v32::Head>(bytes)?.into_current(),
            35.. => decode(bytes)?,
            _ => unreachable!("FORMAT.check enforces oldest..=current"),
        };
        // Runs name every segment: the first from 0, each after the one before, within 0..end.
        let from_zero = head.runs.first().is_none_or(|&(from, _)| from == 0);
        let ascending = head.runs.windows(2).all(|pair| matches!(pair, [(a, _), (b, _)] if a < b));
        let within = head.runs.iter().all(|&(from, _)| from < head.end);
        let named = head.end == 0 || !head.runs.is_empty();
        if !(from_zero && ascending && within && named) {
            return Err(crate::Error::Corrupt(format!(
                "{cell}'s head: runs {:?} not naming every segment to {}, in order",
                head.runs, head.end
            )));
        }
        Ok(head)
    }

    /// Whether the cell was deleted: no writer takes it again.
    pub fn deleted(&self) -> bool {
        self.epoch == Self::FINAL
    }

    /// The epoch of the writer of segment `segment`, if the head names it.
    pub fn epoch_of(&self, segment: u64) -> Option<u64> {
        if segment >= self.end {
            return None;
        }
        let run = self.runs.partition_point(|&(from, _)| from <= segment);
        run.checked_sub(1).map(|run| self.runs[run].1)
    }

    /// The segments the head names from `segment` on, each by its number and its writer's epoch:
    /// those of its runs, each to the next's first or its end. A decoded head's runs name every
    /// segment to its end ([`Head::decode`], board 8545).
    pub fn segments(&self, segment: u64) -> impl Iterator<Item = (u64, u64)> + '_ {
        let end = self.end;
        let next = self.runs.iter().skip(1).map(|&(from, _)| from).chain(std::iter::once(end));
        self.runs.iter().zip(next).flat_map(move |(&(from, epoch), to)| {
            (from.max(segment)..to.min(end)).map(move |segment| (segment, epoch))
        })
    }

    /// The key of segment `segment` of `cell`, if the head names it.
    pub fn segment(&self, cell: CellId, segment: u64) -> Option<String> {
        self.epoch_of(segment).map(|epoch| self::segment(cell, segment, epoch))
    }

    /// The key of `cell`'s newest checkpoint, if it has one.
    pub fn checkpoint(&self, cell: CellId) -> Option<String> {
        self.checkpoint.map(|(segment, epoch)| checkpoint(cell, segment, epoch))
    }

    /// The head with the segments of the writer of `epoch` to `end` named too: the next ones its
    /// writer acknowledges.
    pub fn holding(mut self, epoch: u64, end: u64) -> Self {
        if end > self.end {
            if self.runs.last().is_none_or(|&(_, last)| last != epoch) {
                self.runs.push((self.end, epoch));
            }
            self.end = end;
        }
        self
    }

    /// cladefs: the head with the versions cut at the ends of the segments `cuts` names, each by
    /// its number, named too, in order (doc-18, TASK-476): each after the versions named, and its
    /// window, if it asks for one, keeping it and the newest before it alone.
    pub fn naming<'a>(mut self, cuts: impl IntoIterator<Item = (u64, &'a NewVersion)>) -> Self {
        for (at, cut) in cuts {
            if self.version(cut.id).is_some() {
                continue;
            }
            let NewVersion { id, keep_last } = *cut;
            let epoch = self.epoch_of(at).unwrap_or(self.epoch);
            self.versions.push(Version { id, at, epoch, shared: false });
            if let Some(keep) = keep_last {
                let keep = usize::try_from(keep).unwrap_or(usize::MAX).max(1);
                let dropped = self.versions.len().saturating_sub(keep);
                self.versions.drain(..dropped);
            }
        }
        self
    }

    /// cladefs: the version of ID `id` it names, if it does.
    pub fn version(&self, id: u64) -> Option<&Version> {
        self.versions.iter().find(|version| version.id == id)
    }

    /// cladefs: the head naming none of the versions `ids` names.
    pub fn without(mut self, ids: &BTreeSet<u64>) -> Self {
        self.versions.retain(|version| !ids.contains(&version.id));
        self
    }

    /// cladefs: the head with its version of ID `id` shared, if `shared`, or no longer: other
    /// tenants' cells may fork it while it is (doc-7, Shared versions).
    pub fn sharing(mut self, id: u64, shared: bool) -> Self {
        for version in self.versions.iter_mut().filter(|version| version.id == id) {
            version.shared = shared;
        }
        self
    }
}

/// cladefs: a reference's bytes ([`reference`]): the format of the bucket's objects, first, as a
/// record's, and the fork's owner, its child's ([`Child::owner`], a clone's its restore's): unique
/// to the fork, so that every writer of it writes the same, a writable fork's host, its parent's
/// writers and its parent's deletion alike, and a create landed though its answer was lost is taken
/// for its own (TASK-395.3). What the collector keeps by is the key alone; nothing reads them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub struct Reference {
    pub format: u32,
    pub owner: u64,
}

impl Reference {
    /// The reference of the fork whose child's owner is `owner`.
    pub fn of(owner: u64) -> Self {
        Self { format: FORMAT.current, owner }
    }

    /// Decodes a reference object, checking its format version against [`FORMAT`] (doc-28, board
    /// 10038).
    pub fn decode(bytes: &[u8]) -> Result<Self, crate::Error> {
        let format: u32 = decode(bytes.get(..4).unwrap_or(bytes))?;
        let format = FORMAT.check(format).map_err(|_| crate::Error::BucketFormat(format))?;
        match format {
            31.. => decode(bytes),
            _ => unreachable!("FORMAT.check enforces oldest..=current"),
        }
    }
}

/// Frozen v31..=v33 mirror marker encoding (just the format version; always represents `Marked`).
pub(crate) mod v33 {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
    pub struct Mirror {
        pub format: u32,
    }
}

/// cladefs: a mirror's state at [`mirror`] (v34+, board 9498): `Copying { owner }` while a mirror
/// run owns the marker slot and is copying objects (unmarked), transitioned conditionally
/// (`PutMode::Update`) to `Marked` once every object is copied so a marker write landing past
/// `PIN_STALE` cannot mark a copy another run took over or B's collector deleted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub enum MirrorState {
    Copying { owner: u64 },
    Marked,
}

/// cladefs: a mirror's object's bytes ([`mirror`], TASK-633.8, board 6017, board 9498): its format
/// version and [`MirrorState`]; the checkpoint epoch is carried directly in `Origin`. Versions
/// 31..=33 stored only the format and decode as [`MirrorState::Marked`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub struct Mirror {
    pub format: u32,
    pub state: MirrorState,
}

impl Mirror {
    pub const NOW: Self = Self { format: FORMAT.current, state: MirrorState::Marked };

    pub const fn copying(owner: u64) -> Self {
        Self { format: FORMAT.current, state: MirrorState::Copying { owner } }
    }

    pub const fn is_marked(&self) -> bool {
        matches!(self.state, MirrorState::Marked)
    }

    /// Decodes a mirror object, checking its format version against [`FORMAT`] (doc-28,
    /// board 10038, board 9498).
    pub fn decode(bytes: &[u8]) -> Result<Self, crate::Error> {
        let format: u32 = decode(bytes.get(..4).unwrap_or(bytes))?;
        let format = FORMAT.check(format).map_err(|_| crate::Error::BucketFormat(format))?;
        match format {
            31..=33 => {
                let _: v33::Mirror = decode(bytes)?;
                Ok(Self { format: FORMAT.current, state: MirrorState::Marked })
            }
            34.. => decode(bytes),
            _ => unreachable!("FORMAT.check enforces oldest..=current"),
        }
    }
}

/// cladefs: a reference's mark's bytes ([`retired`]): the format of the bucket's objects alone, the
/// same whoever writes it. What a pass deletes by is its key alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub struct Retired {
    pub format: u32,
}

impl Retired {
    pub const NOW: Self = Self { format: FORMAT.current };

    /// Decodes a retired-reference mark, checking its format version against [`FORMAT`] (doc-28,
    /// board 10038).
    pub fn decode(bytes: &[u8]) -> Result<Self, crate::Error> {
        let format: u32 = decode(bytes.get(..4).unwrap_or(bytes))?;
        let format = FORMAT.check(format).map_err(|_| crate::Error::BucketFormat(format))?;
        match format {
            31.. => decode(bytes),
            _ => unreachable!("FORMAT.check enforces oldest..=current"),
        }
    }
}

pub fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    bincode::serialize(value).expect("objects serialize")
}

/// Decodes a bincode-encoded bucket object, refusing trailing bytes (doc-28, board 10038).
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, crate::Error> {
    let options = bincode::DefaultOptions::new().with_fixint_encoding().reject_trailing_bytes();
    options.deserialize(bytes).map_err(|error| crate::Error::Corrupt(error.to_string()))
}

/// Encodes `value` prefixed with the current bucket layout version ([`FORMAT`], doc-28, board
/// 10038).
pub fn encode_versioned<T: Serialize>(value: &T) -> Vec<u8> {
    let mut bytes = FORMAT.current.to_le_bytes().to_vec();
    bincode::serialize_into(&mut bytes, value).expect("objects serialize");
    bytes
}

/// Splits and checks a leading [`FORMAT`] version prefix on `bytes`.
pub(crate) fn check_version(bytes: &[u8]) -> Result<(u32, &[u8]), crate::Error> {
    let (version, rest) =
        bytes.split_first_chunk::<4>().ok_or_else(|| crate::Error::Corrupt("no version".into()))?;
    let format = u32::from_le_bytes(*version);
    let version = FORMAT.check(format).map_err(|_| crate::Error::BucketFormat(format))?;
    Ok((version, rest))
}

/// Decodes a version-prefixed value written by [`encode_versioned`], checking its version against
/// [`FORMAT`] and refusing trailing bytes (doc-28, board 10038).
pub fn decode_versioned<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, crate::Error> {
    let (version, rest) = check_version(bytes)?;
    match version {
        31.. => decode(rest),
        _ => unreachable!("FORMAT.check enforces oldest..=current"),
    }
}

pub use extent_frame::{Frame, PART, TRAILER, crc};

/// How a writer encrypts what it frames: under its cell's DEK, the newest, and which it is
/// ([`crate::keys::Ring::encrypting`], TASK-411.3). Nothing is framed in the clear (TASK-411.28).
#[cfg(feature = "full")]
pub type EncryptionKey<'a> = (&'a extent_frame::Secret, extent_frame::KeyId);

/// cladefs: borrowed or owned encryption key accepted by framing helpers so revocable `Ring`s can
/// hand out owned `Arc<Secret>` keys without breaking borrowed callers (TASK-637, board 2704).
#[cfg(feature = "full")]
pub trait KeyArg {
    fn as_key(&self) -> EncryptionKey<'_>;
}

#[cfg(feature = "full")]
impl KeyArg for (&extent_frame::Secret, extent_frame::KeyId) {
    fn as_key(&self) -> EncryptionKey<'_> {
        (self.0, self.1)
    }
}

#[cfg(feature = "full")]
impl KeyArg for (std::sync::Arc<extent_frame::Secret>, extent_frame::KeyId) {
    fn as_key(&self) -> EncryptionKey<'_> {
        (&self.0, self.1)
    }
}

#[cfg(feature = "full")]
impl KeyArg for &(std::sync::Arc<extent_frame::Secret>, extent_frame::KeyId) {
    fn as_key(&self) -> EncryptionKey<'_> {
        (&self.0, self.1)
    }
}

/// The bytes of an object of `data` and `footer`, framed (`extent_frame`): the data's parts,
/// compressed, the footer, the table of where the parts are, then the [`TRAILER`]. cladefs:
/// encrypted as `encrypting` says (TASK-411.3).
#[cfg(feature = "full")]
pub fn with_footer<T: Serialize>(data: &[u8], footer: &T, encrypting: impl KeyArg) -> Vec<u8> {
    extent_frame::framed(data, &encode(footer), encrypting.as_key())
}

/// cladefs: the versioned footer of a segment's object (doc-28, board 10038).
pub fn encode_segment_footer(footer: &Footer) -> Vec<u8> {
    footer.encode(FORMAT.current)
}

/// cladefs: decodes a versioned segment footer, checking its version against [`FORMAT`] (doc-28,
/// board 10038).
pub fn decode_segment_footer(bytes: &[u8]) -> Result<Footer, crate::Error> {
    Ok(Footer::decode(bytes, FORMAT)?)
}

/// cladefs: the versioned footer of an extent stored on its own: its version ([`FORMAT`]), then
/// its [`ExtentId`] (TASK-411.3, doc-28, board 10038).
pub fn own_extent_footer(extent: ExtentId) -> Vec<u8> {
    encode_versioned(&extent)
}

/// cladefs: decodes the versioned footer of an extent stored on its own, checking its version
/// against [`FORMAT`] (doc-28, board 10038).
pub fn decode_own_extent_footer(bytes: &[u8]) -> Result<ExtentId, crate::Error> {
    decode_versioned(bytes)
}

/// cladefs: the object of `extent`, stored on its own, of `data`: its footer names it and its
/// bucket layout version, so that one copied to another key, encrypted, is refused (TASK-411.3,
/// doc-28, board 10038), encrypted as `encrypting` says.
#[cfg(feature = "full")]
pub fn own_extent(extent: ExtentId, data: &[u8], encrypting: impl KeyArg) -> Vec<u8> {
    extent_frame::framed(data, &own_extent_footer(extent), encrypting.as_key())
}

/// The data of a whole object [`with_footer`] made, checked against its CRCs, by a reader holding
/// `keys` (TASK-411.3): none of an object encrypted under a key not of them
/// ([`crate::Error::NoKey`]).
pub fn data_under(bytes: &[u8], keys: &dyn extent_frame::Keys) -> Result<Vec<u8>, crate::Error> {
    Ok(extent_frame::data(bytes, keys)?)
}

impl Segment {
    /// Its object, cladefs: encrypted as `encrypting` says (TASK-411.3), its footer versioned
    /// (doc-28, board 10038).
    #[cfg(feature = "full")]
    pub fn encode(&self, encrypting: impl KeyArg) -> Vec<u8> {
        extent_frame::framed(&self.data, &encode_segment_footer(&self.footer), encrypting.as_key())
    }

    /// The footer of a segment's object, checked against its CRC and [`FORMAT`].
    pub fn footer(bytes: &[u8]) -> Result<Footer, crate::Error> {
        Self::footer_under(bytes, &extent_frame::NoKeys)
    }

    /// The footer of a segment's object, checked against its MAC under `keys` and [`FORMAT`].
    pub fn footer_under(
        bytes: &[u8],
        keys: &dyn extent_frame::Keys,
    ) -> Result<Footer, crate::Error> {
        decode_segment_footer(extent_frame::footer(bytes, keys)?)
    }
}

#[cfg(all(test, feature = "full"))]
mod tests {
    use super::*;

    /// TASK-395.2: a delimiter listing of a cell's references gives the parents with forks, and one
    /// of a parent's, its distinct origins: each prefix read back as what it names, and no
    /// other.
    #[test]
    fn references_list_by_origin() {
        let (parent, child) = (CellId { tenant: 0xa, id: 0xc }, CellId { tenant: 1, id: 5 });
        let key = reference(parent, 7, child);
        assert!(key.starts_with(&origin(parent, 7)));
        assert!(origin(parent, 7).starts_with(&references(parent)));
        assert_eq!(referenced_at(parent, &origin(parent, 7)), Some(7));
        assert_eq!(referenced_at(parent, &references(parent)), None);
        let other = CellId { tenant: 0xa, id: 0xd };
        assert_eq!(referenced_at(other, &origin(parent, 7)), None);
    }

    #[test]
    fn kinds() {
        let cell = CellId { tenant: 0xa, id: 0xc };
        let id = ExtentId::new(3, 9);
        assert_eq!(kind(&segment(cell, 7, 2)), Kind::Segment { cell, segment: 7, epoch: 2 });
        let own = Kind::Extent { cell, epoch: 2, segment: 4, sequence: 9 };
        assert_eq!(kind(&extent(cell, 2, 4, id)), own);
        // A key naming no segment declares nothing: no extent's.
        let earlier = format!("{}extents/0000000000003/0000000000002.0000000000009", cell.prefix());
        assert_eq!(kind(&earlier), Kind::Unknown);
        assert_eq!(kind(&checkpoint(cell, 7, 2)), Kind::Checkpoint { cell, segment: 7, epoch: 2 });
        assert_eq!(kind(&head(cell)), Kind::Head { cell });
        assert_eq!(objectid_block(0x2a), "objectid/000000000001a");
        assert_eq!(kind(&objectid_block(0x2a)), Kind::ObjectIds { block: 0x2a });
        assert_eq!(kind("objectid/2a"), Kind::Unknown);
        assert_eq!((kind(IDENTITY), kind(IDENTITY).name()), (Kind::Identity, "identity"));
        assert_eq!(kind("identity/1"), Kind::Unknown);
        assert_eq!((kind(BUCKETS), kind(BUCKETS).name()), (Kind::Buckets, "buckets"));
        assert_eq!(kind("tenants/2a/encrypted"), Kind::Unknown);
        assert_eq!(kek(0x2a), "tenants/000000000001a/kek");
        assert_eq!(kind(&kek(0x2a)), Kind::Kek { tenant: 0x2a });
        assert_eq!((kind(&kek(0x2a)).name(), owner(&kek(0x2a))), ("kek", None));
        assert_eq!(kind("tenants/2a/kek"), Kind::Unknown);
        assert_eq!(kind(&format!("{}kek", cell.prefix())), Kind::Unknown);
        assert_eq!(kind(&format!("{}encrypted", cell.prefix())), Kind::Unknown);
        assert_eq!(kind(&collector_lease(cell)), Kind::CollectorLease { cell });
        // cladefs: what mark-only passes marked, which went (board 10545): of no kind now.
        assert_eq!(kind(&format!("{}gc/marks/000000000001a", cell.prefix())), Kind::Unknown);
        assert_eq!(kind(&format!("{}gc/other", cell.prefix())), Kind::Unknown);
        // cladefs: format 27's moves are Kind::Unknown (board 10612); a head's slots of format 28
        // are refused where found (doc-27).
        let of = "segments/0000000000002.0000000000007";
        let moves = format!("{}moves/{of}/0000000000001", cell.prefix());
        assert_eq!(kind(&moves), Kind::Unknown);
        let slot = format!("{}head/0000000000003", cell.prefix());
        assert_eq!(kind(&slot), Kind::Former { cell, format: 28 });
        let child = CellId { tenant: 1, id: 5 };
        assert_eq!(
            kind(&reference(cell, 7, child)),
            Kind::Reference { parent: cell, at: 7, child }
        );
        assert_eq!(kind(&retired(cell, 7, child)), Kind::Retired { parent: cell, at: 7, child });
        assert_eq!(retired_of(&reference(cell, 7, child)), Some(retired(cell, 7, child)));
        assert_eq!(retired_of(&retired(cell, 7, child)), None);
        assert!(!retired(cell, 7, child).starts_with(&references(cell)));
        // cladefs: a mirror's pin and hold — both v33 per-run and legacy v31/v32 single-key forms
        // (TASK-633.8, board 11699) — and its marker.
        assert_eq!(kind(&legacy_pin(cell, 7)), Kind::Pin { parent: cell, at: 7 });
        assert_eq!(kind(&pin(cell, 7, 0)), Kind::Pin { parent: cell, at: 7 });
        assert_eq!(kind(&pin(cell, 7, 0x2a)), Kind::Pin { parent: cell, at: 7 });
        assert_eq!(kind(&pin(cell, 7, u64::MAX)), Kind::Pin { parent: cell, at: 7 });
        assert!(pins(cell, 7).ends_with('/'));
        assert!(pin(cell, 7, 0x2a).starts_with(&pins(cell, 7)));
        assert_eq!(kind(&legacy_hold(cell, 7, 3)), Kind::Hold { parent: cell, at: 7, bucket: 3 });
        assert_eq!(kind(&hold(cell, 7, 3, 0x2a)), Kind::Hold { parent: cell, at: 7, bucket: 3 });
        assert!(holds(cell, 7, 3).ends_with('/'));
        assert!(hold(cell, 7, 3, 0x2a).starts_with(&holds(cell, 7, 3)));
        assert_eq!(
            hold(cell, 7, 0x1234, 0x2a),
            format!("{}mirror-holds/{}/{}", origin(cell, 7), Padded(0x1234), Padded(0x2a))
        );
        assert_eq!(hold_of_pin(&legacy_pin(cell, 7), 3), Some(legacy_hold(cell, 7, 3)));
        assert_eq!(hold_of_pin(&pin(cell, 7, 0x2a), 3), Some(hold(cell, 7, 3, 0x2a)));
        assert_eq!(hold_of_pin(&hold(cell, 7, 3, 0x2a), 3), None);
        assert_eq!(
            kind(&format!("{}mirror.0003", origin(cell, 7))),
            Kind::Unknown,
            "hex bucket numbers are refused after format 31"
        );
        assert_eq!(
            kind(&format!("{}mirror.{}", origin(cell, 7), Padded(u64::from(u16::MAX) + 1))),
            Kind::Unknown,
            "bucket numbers past u16::MAX are refused"
        );
        for malformed in [
            format!("{}mirror-pins/bad", origin(cell, 7)),
            format!("{}mirror-pins/{}/extra", origin(cell, 7), Padded(1)),
            format!("{}mirror-holds/{}/bad", origin(cell, 7), Padded(3)),
            format!("{}mirror-holds/{}/{}/extra", origin(cell, 7), Padded(3), Padded(1)),
            format!(
                "{}mirror-holds/{}/{}",
                origin(cell, 7),
                Padded(u64::from(u16::MAX) + 1),
                Padded(1)
            ),
        ] {
            assert_eq!(kind(&malformed), Kind::Unknown, "{malformed}");
        }
        assert_eq!(owner(&hold(cell, 7, 3, 0x2a)), Some(cell));
        assert!(pin(cell, 7, 0x2a).starts_with(&origin(cell, 7)));
        assert_eq!(retired_of(&pin(cell, 7, 0x2a)), None);
        assert_eq!(kind(&mirror(cell, 7)), Kind::Mirror { cell, at: 7 });
        // cladefs: an ephemeral cell's reference, among its version's, and its index (doc-25).
        let ephemeral = ephemeral_reference(cell, 7, 0x2a, child);
        assert!(ephemeral.starts_with(&origin(cell, 7)));
        assert_eq!(
            kind(&ephemeral),
            Kind::Ephemeral { parent: cell, at: 7, incarnation: 0x2a, child }
        );
        assert_eq!((owner(&ephemeral), retired_of(&ephemeral)), (Some(cell), None));
        // Its index entry, in its parent's bucket, and its incarnation's tombstone beside the
        // entries' prefix, never under it (boards 8517, 8519), its bucket in base 32 (board 10038).
        let index = ephemeral_index(0x2a, cell, 7, child);
        assert!(index.starts_with(&ephemeral_indexes(ephemeral_bucket(cell), 0x2a)));
        assert_eq!(ephemeral_indexed(&index), Some((0x2a, cell, 7, child)));
        assert_eq!(kind(&index), Kind::EphemeralIndex { incarnation: 0x2a });
        assert_eq!(owner(&index), None);
        let theirs = CellId { tenant: 1, id: 3 << 48 | 5 };
        let there = ephemeral_index(0x2a, theirs, 7, child);
        assert_eq!(
            there,
            "ephemeral/0000000000003/000000000001a/0000000000001.000r000000005.0000000000007.0000000000001.0000000000005"
        );
        assert_eq!((routed_to(&there), routed_to(&ephemeral_hosts(3))), (Some(3), Some(3)));
        assert_eq!(routed_to(&index), None, "a named cell's stays in the bucket asked");
        let forged = there.replacen("/0000000000003/", "/0000000000002/", 1);
        assert_eq!((ephemeral_indexed(&forged), kind(&forged)), (None, Kind::Unknown));
        let tombstone = decommissioned(3, 0x2a);
        assert_eq!(tombstone, "ephemeral/0000000000003/000000000001a.decommissioned");
        assert!(!tombstone.starts_with(&ephemeral_indexes(3, 0x2a)));
        assert!(tombstone.starts_with(&ephemeral_hosts(3)));
        assert_eq!(decommissioned_of(&tombstone), Some((3, 0x2a)));
        assert_eq!(kind(&tombstone), Kind::Decommissioned { incarnation: 0x2a });
        assert_eq!((owner(&tombstone), routed_to(&tombstone)), (None, Some(3)));
        for malformed in [
            "ephemeral/3/000000000001a.decommissioned",
            "ephemeral/0003/000000000001a.decommissioned",
            "ephemeral/+003/000000000001a.decommissioned",
            "ephemeral/0000000002000/000000000001a.decommissioned",
            "ephemeral/0000000000003/1a.decommissioned",
            "ephemeral/0000000000003/000000000001a.decommissione",
        ] {
            assert_eq!(kind(malformed), Kind::Unknown, "{malformed}");
        }
        assert!(mirror(cell, 7).starts_with(&mirrors(cell)));
        assert_eq!((owner(&pin(cell, 7, 0x2a)), owner(&mirror(cell, 7))), (Some(cell), Some(cell)));
        assert_eq!(kind(&format!("{}mirrors/x", cell.prefix())), Kind::Unknown);
        assert_eq!(kind(&format!("{}2", origin(cell, 7))), Kind::Unknown);
        assert_eq!(kind(&origin(cell, 7)), Kind::Unknown);
        // cladefs: a version's read evidence, each tenant's under its own prefix (TASK-702.2).
        let evidence = evidence(cell, 7, 3, 0x2a);
        assert_eq!(
            evidence,
            "tenants/000000000000a/cells/000000000000c/reads/0000000000007/0000000000003/000000000001a"
        );
        assert!(evidence.starts_with(&evidence_of(cell, 7, 3)));
        assert!(evidence_of(cell, 7, 3).starts_with(&evidence_of_cell(cell)));
        assert_eq!(kind(&evidence), Kind::Evidence { parent: cell, at: 7, tenant: 3 });
        assert_eq!(owner(&evidence), Some(cell));
        assert_eq!(kind(&evidence_of(cell, 7, 3)), Kind::Unknown);
        assert_eq!(kind(&format!("{}x", evidence_of(cell, 7, 3))), Kind::Unknown);
        // cladefs: fast WAL segments under a cell's prefix (doc-29).
        let wal_key = wal(cell, 2, 7);
        assert!(wal_key.starts_with(&wal_epoch_prefix(cell, 2)));
        assert!(wal_epoch_prefix(cell, 2).starts_with(&wal_prefix(cell)));
        assert_eq!(kind(&wal_key), Kind::Wal { cell, epoch: 2, segment: 7 });
        assert_eq!(owner(&wal_key), Some(cell));
        assert_eq!(kind(&format!("{}x", wal_prefix(cell))), Kind::Unknown);
        // The layout before cells, and keys of cells' prefixes malformed, are no objects of a
        // cell; nor the collector's lease, marks and moves at the bucket's root, as format 8 had
        // them (TASK-395.2).
        assert_eq!(kind("segments/0000000000003/0000000000007"), Kind::Unknown);
        assert_eq!(kind("cells/0000000000003"), Kind::Unknown);
        assert_eq!(kind("tenants/a/cells/000000000000c/cell"), Kind::Unknown);
        assert_eq!(kind("tenants/000000000000a/cell/000000000000c/cell"), Kind::Unknown);
        let short = format!("{}segments/0000000000003/7", cell.prefix());
        assert_eq!(kind(&short), Kind::Unknown);
        // A segment's or a checkpoint's key names its writer's epoch, since format 10 (TASK-407).
        let unwritten = format!("{}segments/0000000000003/0000000000007", cell.prefix());
        assert_eq!(kind(&unwritten), Kind::Unknown);
        assert_eq!(kind("gc/lease"), Kind::Unknown);
        assert_eq!(kind("gc/marks/000000000001a"), Kind::Unknown);
        assert_eq!(kind(&format!("moves/{}", segment(cell, 7, 2))), Kind::Unknown);
        assert_eq!(cell_of_prefix(&cell.prefix()), Some(cell));
        assert_eq!(cell_of_prefix(&head(cell)), None);
        assert_eq!(cell_of_prefix(""), None);
    }

    /// TASK-394, TASK-395.2: every kind of a cell's objects is the cell's, the head slots of
    /// formats before 29 too, its collector's lease, and the references under it, of forks of its
    /// cells to other cells; object-ID blocks and keys of no kind are no cell's.
    #[test]
    fn owners() {
        let cell = CellId { tenant: 0xa, id: 0xc };
        let child = CellId { tenant: 1, id: 5 };
        let keys = [
            segment(cell, 7, 2),
            extent(cell, 2, 4, ExtentId::new(3, 9)),
            checkpoint(cell, 7, 2),
            head(cell),
            wal(cell, 2, 7),
            format!("{}head/0000000000003", cell.prefix()),
            collector_lease(cell),
            reference(cell, 7, child),
            retired(cell, 7, child),
        ];
        for key in keys {
            assert_eq!(owner(&key), Some(cell), "{key}");
        }
        let keys = [
            "gc/lease".to_owned(),
            "gc/marks/0000000000001".to_owned(),
            objectid_block(1),
            IDENTITY.to_owned(),
            BUCKETS.to_owned(),
            "probe/0000000000001".to_owned(),
            "moves/probe/0000000000001".to_owned(),
            format!("{}moves/segments/0000000000002.0000000000007/0000000000001", cell.prefix()),
            format!("{}heads/3", cell.prefix()),
        ];
        for key in keys {
            assert_eq!(owner(&key), None, "{key}");
        }
    }

    /// board 8545: a head's runs, forged, its end too, of every edge (0, 1, the end and its
    /// neighbours, u64::MAX) in every order of up to three: decoded only if they name every segment
    /// of 0..end, in order, else refused as corrupt; and each decoded names, of its segments, those
    /// its runs give, its last few read as `epoch_of` reads them, never a panic.
    #[test]
    fn heads_whose_runs_name_not_every_segment_are_refused() {
        let cell = test_cell(3);
        for end in [0, 1, 2, 3, u64::MAX - 1, u64::MAX] {
            let edges = [0, 1, end.saturating_sub(1), end, end.saturating_add(1), u64::MAX];
            let mut cases: Vec<Vec<u64>> = vec![vec![]];
            for _ in 0..3 {
                let longer: Vec<Vec<u64>> = (cases.iter())
                    .flat_map(|case| {
                        edges.iter().map(move |&edge| [case.clone(), vec![edge]].concat())
                    })
                    .collect();
                cases.extend(longer);
            }
            cases.sort();
            cases.dedup();
            for froms in cases {
                let runs: Vec<(u64, u64)> =
                    (froms.iter()).zip(10..).map(|(&f, e)| (f, e)).collect();
                let head =
                    Head { end, runs: runs.clone(), ..Head::of(crate::record::Made::Own, 7) };
                let valid = froms.first().is_none_or(|&first| first == 0)
                    && froms.windows(2).all(|pair| pair[0] < pair[1])
                    && froms.iter().all(|&from| from < end)
                    && (end == 0 || !froms.is_empty());
                let decoded = Head::decode(cell, &encode(&head));
                if !valid {
                    assert!(matches!(decoded, Err(crate::Error::Corrupt(_))), "{end} {runs:?}");
                    continue;
                }
                let decoded = decoded.unwrap_or_else(|err| panic!("{end} {runs:?}: {err}"));
                let from = end.saturating_sub(3);
                let named: Vec<(u64, u64)> = decoded.segments(from).take(3).collect();
                let read: Vec<(u64, u64)> = (from..end)
                    .map(|segment| (segment, decoded.epoch_of(segment).unwrap()))
                    .collect();
                assert_eq!(named, read, "{end} {runs:?}");
                assert_eq!(
                    decoded.segments(0).next().map(|(segment, _)| segment),
                    (end > 0).then_some(0)
                );
            }
        }
    }

    /// TASK-476: a head names the versions cut at the segments it names, in order, each once; a
    /// window keeps the version cutting it and the newest before it alone; and a version is
    /// dropped, or shared, by its ID. cladefs: each at the epoch of its segment's writer, the
    /// head's own for a segment it names not (board 6017).
    #[test]
    fn heads_name_their_versions() {
        let cut = |id, keep_last| NewVersion { id, keep_last };
        let named = |head: &Head| -> Vec<(u64, u64)> {
            head.versions.iter().map(|version| (version.id, version.at)).collect()
        };
        let head = Head::default().naming([(2, &cut(10, None)), (3, &cut(11, None))]);
        assert_eq!(named(&head), [(10, 2), (11, 3)]);
        assert_eq!(named(&head.clone().naming([(2, &cut(10, None))])), named(&head), "once");
        let windowed = head.clone().naming([(4, &cut(12, Some(2)))]);
        assert_eq!(named(&windowed), [(11, 3), (12, 4)]);
        let alone = head.clone().naming([(4, &cut(12, Some(0)))]);
        assert_eq!(named(&alone), [(12, 4)], "a window keeps its own version, at least");
        let dropped = windowed.clone().without(&BTreeSet::from([11]));
        assert_eq!(named(&dropped), [(12, 4)]);
        let twelve = Version { id: 12, at: 4, epoch: 0, shared: false };
        assert_eq!(windowed.version(12), Some(&twelve));
        assert!(windowed.clone().sharing(12, true).version(12).unwrap().shared);
        assert_eq!(windowed.version(10), None);
        let held = Head { epoch: 5, ..Head::default() }.holding(3, 2);
        let versions = held.naming([(1, &cut(13, None)), (4, &cut(14, None))]).versions;
        assert_eq!(versions.iter().map(|version| version.epoch).collect::<Vec<_>>(), [3, 5]);
    }

    /// TASK-407: a head names each of its segments by its writer's epoch, from its runs, a run a
    /// takeover; and none past its end. A writer's segments it names next go on its last run if
    /// it is the writer's, or start one.
    #[test]
    fn heads_name_their_segments_by_epoch() {
        let cell = test_cell(1);
        let head = Head { end: 0, ..Head::default() }.holding(1, 3).holding(1, 4).holding(3, 6);
        assert_eq!((head.end, &head.runs[..]), (6, &[(0, 1), (4, 3)][..]));
        let named: Vec<_> = head.segments(2).collect();
        assert_eq!(named, [(2, 1), (3, 1), (4, 3), (5, 3)]);
        assert_eq!(
            (head.epoch_of(3), head.epoch_of(4), head.epoch_of(6)),
            (Some(1), Some(3), None)
        );
        assert_eq!(head.segment(cell, 5), Some(segment(cell, 5, 3)));
        assert_eq!(head.clone().holding(3, 5), head, "nothing named twice, nor taken back");
        let checkpointed = Head { checkpoint: Some((4, 3)), ..head };
        assert_eq!(checkpointed.checkpoint(cell), Some(checkpoint(cell, 4, 3)));
    }
}
