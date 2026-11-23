// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Where cells keep what, in a bucket, and what the objects hold (doc-7, Layout).
//!
//! Numbers in keys are fixed-width base 32 ([`Padded`]), so keys list in numeric order. Objects are
//! read from a bucket, by ranged reads, as [`crate::object`] reads them.

use base32_id::Padded;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use storage_device::ExtentId;

pub use extent_frame::segment::{
    CellId, Chain, Contents, Continued, Footer, Location, NewVersion, epoch_of, tag, writer_of,
};

/// cladefs: a fork's child: its cell, and its owner, which every record of the child names,
/// whoever writes it (TASK-79.82, TASK-153.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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
/// prefix: its objects' moves ([`moves`]), its collector's lease and marks ([`collector_lease`],
/// [`marks`]), and the references of forks of its cells ([`reference`]), so that a pass of the
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
/// of objects, its slots, each written in turn ([`head`], [`Head::swaps`], TASK-691), as GCS takes
/// about one write a second of an object, and a bucket's identity holds its settings
/// ([`crate::identity::Settings`]); nothing reads 22's, whose head was one object. 24: a version's
/// read evidence, what forks of it read of it as they opened, each tenant's under the version's
/// cell's prefix ([`evidence`], [`crate::evidence`], TASK-702.2); nothing reads 23's. 25: an origin
/// and a version name the epoch of their segment, whose checkpoint is read by its key, with no read
/// of its cell's head (board 6017); nothing reads 24's. 26: a checkpoint keeps the length of the
/// data of each of its cell's own segments its extents are in ([`crate::State::lengths`], doc-27),
/// which its writer measures their density against; nothing reads 25's.
pub const FORMAT: u32 = 26;

/// cladefs: the most slots a cell's head is kept in ([`crate::identity::Settings::head_slots`],
/// TASK-691): keys past it are no head's.
pub const MAX_HEAD_SLOTS: u64 = 64;

/// cladefs: the prefix of every tenant's cells' objects: all the bucket's cells (TASK-364).
pub const TENANTS: &str = "tenants/";
/// cladefs: under a cell's prefix, the collector's own: its lease, its marks, and what else its
/// passes over the cell keep there; never a pass's candidate (TASK-395.2).
const GC: &str = "gc/";
const GC_LEASE: &str = "gc/lease";
const GC_MARKS: &str = "gc/marks/";
/// Under a cell's prefix, the moves of its objects, and the moves of those, `moves/moves/...`.
const MOVES: &str = "moves/";
/// cladefs: under a cell's prefix, the references of forks of its cells ([`reference`]).
const REFS: &str = "refs/";
/// cladefs: under a cell's prefix, the marks of the references retired or taken back ([`retired`]).
const RETIRED: &str = "retired/";
/// cladefs: under a cell's prefix in another bucket, the markers of its versions mirrored there
/// ([`mirror`], TASK-633.8).
const MIRRORS: &str = "mirrors/";
/// cladefs: the last part of a mirror's pin's key, where a reference's names its child ([`pin`]).
const PIN: &str = "mirror";
/// cladefs: where an ephemeral cell's reference names its host's incarnation, before its child
/// ([`ephemeral_reference`], doc-25); and, at the bucket's root, the prefix of the index of those
/// references by bucket and incarnation ([`ephemeral_index`]) and of incarnations' tombstones
/// ([`decommissioned`]).
const EPHEMERAL: &str = "ephemeral";
/// cladefs: the last word of an incarnation's tombstone ([`decommissioned`], board 8519).
const DECOMMISSIONED: &str = "decommissioned";
/// cladefs: under a cell's prefix, the read evidence of its versions ([`evidence`], TASK-702.2).
const READS: &str = "reads/";
/// cladefs: the prefix of the blocks of object IDs taken ([`objectid_block`]).
const OBJECTIDS: &str = "objectid/";
/// cladefs: the bucket's identity object: its format and its number, written once by its formatting
/// ([`crate::identity`], TASK-633.9). No cell's, and no candidate of any pass.
pub const IDENTITY: &str = "identity";
/// cladefs: the record of every bucket, the root bucket's own or a copy of it ([`crate::identity`],
/// TASK-633.9). No cell's, and no candidate of any pass.
pub const BUCKETS: &str = "buckets";
/// cladefs: there while a mark-only pass marks the bucket, which it writes and an operator removes
/// with the marks: the one thing readers read to know whether to watch any cell's marks
/// ([`crate::marks`], TASK-395.4's review). No cell's, and no candidate of any pass.
pub const MARKING: &str = "gc/marking";

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

/// The slot `slot` of the head of `cell` ([`Head`]): what made the cell, its keys, its writer's
/// taking, the segments it holds, its newest checkpoint and its versions, swapped with a condition
/// (TASK-407, TASK-515), in its bucket's count of objects, written in turn (TASK-691).
pub fn head(cell: CellId, slot: u64) -> String {
    debug_assert!(slot < MAX_HEAD_SLOTS, "slot {slot} of {MAX_HEAD_SLOTS} at most");
    format!("{}{}", heads(cell), Padded(slot))
}

/// cladefs: the prefix of the slots of the head of `cell` ([`head`], TASK-691).
pub fn heads(cell: CellId) -> String {
    format!("{}head/", cell.prefix())
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

/// What the pass of the collector of epoch `pass` made of the object at `key` when it compacted
/// it: the extents of it that were live, and where in it each is. Being one object, written whole,
/// it is never there without them. cladefs: under the cell of the object, `<C>moves/<rest>/<pass>`
/// for the object `<C><rest>`, and so `<C>moves/moves/<rest>/<pass>/<pass>` for its moves' moves,
/// so that the cell's pass of the collector lists them with the cell (TASK-395.2). The pass's epoch
/// in the key, the cell's collector lease's, two passes' moves of one object never share a key:
/// one slow to create them, after later passes made them and deleted them, makes its own, never
/// again those at a key once read (rule `moves_by_pass`, TASK-407.1); readers find an object's
/// moves by listing [`moves_of`]. Only a cell's objects are compacted: a key of no cell's, which a
/// location read from a checkpoint or a footer may name if they are corrupt, has none
/// ([`crate::Error::Corrupt`]).
pub fn moves(key: &str, pass: u64) -> Result<String, crate::Error> {
    let pass = if crate::rules().moves_by_pass { pass } else { 0 };
    Ok(format!("{}{}", moves_of(key)?, Padded(pass)))
}

/// cladefs: the prefix of the keys of the moves of the object at `key`, whichever passes made them
/// ([`moves`]).
pub fn moves_of(key: &str) -> Result<String, crate::Error> {
    Ok(format!("{}/", moves_under(key)?))
}

/// cladefs: the object whose moves' keys `prefix` is the prefix of, as [`moves_of`] makes it: none
/// for another prefix, of no cell's, or of the moves of several objects ([`moves_under`]), which
/// names no object of a cell's.
pub fn moved_object(prefix: &str) -> Option<String> {
    let (cell, rest) = in_cell(prefix)?;
    let of = rest.strip_prefix(MOVES)?.strip_suffix('/')?;
    let key = format!("{}{of}", cell.prefix());
    let moved = matches!(kind(&key), Kind::Segment { .. } | Kind::Extent { .. });
    // Of no cell's object: no moves' prefix.
    (moved && moves_of(&key).ok()? == prefix).then_some(key)
}

/// cladefs: the prefix of the keys of the moves of the objects whose keys start with `prefix`, a
/// cell's: `<C>moves/<rest>` for `<C><rest>`.
pub fn moves_under(prefix: &str) -> Result<String, crate::Error> {
    let Some((cell, rest)) = in_cell(prefix) else {
        return Err(crate::Error::Corrupt(format!(
            "{prefix}: no cell's object, which has no moves"
        )));
    };
    Ok(format!("{}{MOVES}{rest}", cell.prefix()))
}

/// cladefs: the longest key, in bytes, the buckets cells live in take (S3, GCS, Azure; a macOS file
/// bucket's PATH_MAX): a request of a longer one each refuses, for good (board 5861).
pub const MAX_KEY: usize = 1024;

/// cladefs: how many times the object at `key` is moves of moves: 0 for one not moves, 1 for
/// an object's moves, 2 for their moves, and so on, each in its key once more ([`moves`]).
pub fn moves_depth(key: &str) -> usize {
    let Some((_, mut rest)) = in_cell(key) else { return 0 };
    let mut depth = 0;
    while let Some(under) = rest.strip_prefix(MOVES) {
        (rest, depth) = (under, depth + 1);
    }
    depth
}

/// cladefs: the key of the object the moves at `key` are of, if they are moves: the inverse of
/// [`moves`].
pub fn moved_from(key: &str) -> Option<String> {
    match kind(key) {
        Kind::Moves { cell, of, .. } => Some(format!("{}{of}", cell.prefix())),
        _ => None,
    }
}

/// cladefs: the collector's lease over `cell`, which its passes over the cell take (doc-7, Garbage
/// collection; TASK-395.2), a [`crate::passed::CollectorLease`]: passes over other
/// cells run beside them, unfenced.
pub fn collector_lease(cell: CellId) -> String {
    format!("{}{GC_LEASE}", cell.prefix())
}

/// cladefs: the prefix of the collector's own objects under `cell`: its lease, which holds what its
/// passes record ([`crate::passed`]), and its marks; never a pass's candidate, nor what a pass
/// decides by but its lease.
pub fn collectors(cell: CellId) -> String {
    format!("{}{GC}", cell.prefix())
}

/// cladefs: the prefix of the marks of the mark-only passes over `cell` ([`marks`]).
pub fn marks_of(cell: CellId) -> String {
    format!("{}{GC_MARKS}", cell.prefix())
}

/// cladefs: what the mark-only pass over `cell` of the collector that took the cell's lease at
/// `epoch` found no live cell needs, and would have deleted ([`Marks`]; doc-7, Garbage
/// collection; TASK-264): keyed by the epoch, so that the newest lists last.
pub fn marks(cell: CellId, epoch: u64) -> String {
    format!("{}{}", marks_of(cell), Padded(epoch))
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
    format!("{EPHEMERAL}/{bucket:04x}/")
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
/// it, four hexadecimal digits, alone.
fn ephemeral_under(key: &str) -> Option<(u16, &str)> {
    let rest = key.strip_prefix(EPHEMERAL)?.strip_prefix('/')?;
    let (bucket, rest) = rest.split_once('/')?;
    // Not a bucket's number: no key of the index.
    let number = u16::from_str_radix(bucket, 16).ok()?;
    (format!("{number:04x}") == bucket).then_some((number, rest))
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

/// cladefs: the pin of the mirror of `parent`'s version at the end of its segment `at` in another
/// bucket (doc-23, Mirrors; TASK-633.8): a reference of the mirror's own, among those of the
/// version's children there, which keeps the copy from that bucket's collector while it is made,
/// as a child's reference keeps what it inherits.
pub fn pin(parent: CellId, at: u64) -> String {
    format!("{}{PIN}", origin(parent, at))
}

/// cladefs: the hold of the mirror of `parent`'s version at the end of its segment `at` in the
/// bucket numbered `bucket`, in the parent's bucket (doc-23, Mirrors; TASK-633.8): a reference of
/// the mirror's own, among those of the version's children there, which keeps the version's state
/// there whole, and the parent's cell from being removed, with what the version inherits from
/// other cells, while the copy stands: its children, referring in the mirror's bucket, read there
/// what the mirror copied, and in their cells what it did not. The mirror writes it before it reads
/// the parent's head; the mirror's bucket's collector deletes it, the copy going.
pub fn hold(parent: CellId, at: u64, bucket: u16) -> String {
    format!("{}{PIN}.{bucket:04x}", origin(parent, at))
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

/// cladefs: the prefix of the moves of `cell`'s objects ([`moves`]).
pub fn moves_prefix(cell: CellId) -> String {
    format!("{}{MOVES}", cell.prefix())
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

/// Whether the object at `later` is further along the way of an extent from the object at `key`:
/// the moves of it, or of moves of it.
pub fn further(key: &str, later: &str) -> bool {
    let mut at = later.to_owned();
    while let Some(from) = moved_from(&at) {
        if from == key {
            return true;
        }
        at = from;
    }
    false
}

/// What kind of object a key names, and what its key says. cladefs: an extent's says the segment
/// that names it, `segment`, of the writer of `epoch`, which uploaded it before: what it may yet be
/// named by, as its writer declares it (TASK-79.3, TASK-153.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind<'a> {
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
    /// cladefs: a slot of a cell's head ([`head`], TASK-691).
    Head {
        cell: CellId,
        slot: u64,
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
    /// cladefs: a mark-only pass's word that it marks the bucket ([`MARKING`]).
    Marking,
    /// cladefs: a mark-only pass's marks over a cell ([`marks`]): the collector's own, never a
    /// candidate.
    Marks {
        cell: CellId,
        epoch: u64,
    },
    /// The moves of the object `of`, the rest of its key under the cell's prefix, by the pass of
    /// the collector of epoch `pass` ([`moves`], [`moved_from`]).
    Moves {
        cell: CellId,
        of: &'a str,
        pass: u64,
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
    Unknown,
}

impl Kind<'_> {
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
            Self::Marks { .. } => "marks",
            Self::Marking => "marking",
            Self::Moves { .. } => "moves",
            Self::Reference { .. } => "reference",
            Self::Retired { .. } => "retired",
            Self::Pin { .. } => "pin",
            Self::Hold { .. } => "hold",
            Self::Mirror { .. } => "mirror",
            Self::Ephemeral { .. } => "ephemeral",
            Self::EphemeralIndex { .. } => "ephemeral-index",
            Self::Decommissioned { .. } => "decommissioned",
            Self::Evidence { .. } => "evidence",
            Self::Unknown => "unknown",
        }
    }
}

/// cladefs: the cell whose object `key` is, if a cell's: every kind under its prefix, the moves of
/// its objects and of their moves, its collector's lease and marks, and the references of forks of
/// its cells, under the parent's cell (`cladefs-gc --usage`, TASK-394, TASK-395.2).
pub fn owner(key: &str) -> Option<CellId> {
    match kind(key) {
        Kind::Segment { cell, .. }
        | Kind::Extent { cell, .. }
        | Kind::Checkpoint { cell, .. }
        | Kind::Head { cell, .. }
        | Kind::Reference { parent: cell, .. }
        | Kind::Retired { parent: cell, .. }
        | Kind::Pin { parent: cell, .. }
        | Kind::Hold { parent: cell, .. }
        | Kind::Ephemeral { parent: cell, .. }
        | Kind::Evidence { parent: cell, .. }
        | Kind::Mirror { cell, .. } => Some(cell),
        Kind::CollectorLease { cell } | Kind::Marks { cell, .. } => Some(cell),
        Kind::Moves { cell, .. } => Some(cell),
        Kind::ObjectIds { .. }
        | Kind::EphemeralIndex { .. }
        | Kind::Decommissioned { .. }
        | Kind::Identity
        | Kind::Buckets
        | Kind::Kek { .. }
        | Kind::Marking
        | Kind::Unknown => None,
    }
}

/// The number a key's part writes ([`Padded`]).
fn number(text: &str) -> Option<u64> {
    base32_id::parse_padded(text)
}

pub fn kind(key: &str) -> Kind<'_> {
    let parsed = || -> Option<Kind<'_>> {
        if key == MARKING {
            return Some(Kind::Marking);
        }
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
        if let Some(moves) = rest.strip_prefix(MOVES) {
            let (of, pass) = moves.rsplit_once('/')?;
            return (!of.is_empty()).then_some(Kind::Moves { cell, of, pass: number(pass)? });
        }
        if let Some(rest) = rest.strip_prefix(GC) {
            if rest == GC_LEASE.strip_prefix(GC)? {
                return Some(Kind::CollectorLease { cell });
            }
            let epoch = rest.strip_prefix(GC_MARKS.strip_prefix(GC)?)?;
            return Some(Kind::Marks { cell, epoch: number(epoch)? });
        }
        // A reference's key, or its mark's, the rest after their prefixes alike.
        let fork = |rest: &str| -> Option<(CellId, u64, CellId)> {
            let (at, child) = rest.split_once('/')?;
            let (tenant, child) = child.split_once('.')?;
            let child = CellId { tenant: number(tenant)?, id: number(child)? };
            Some((cell, number(at)?, child))
        };
        if let Some(rest) = rest.strip_prefix(REFS) {
            if let Some(at) = rest.strip_suffix(PIN).and_then(|at| at.strip_suffix('/')) {
                return Some(Kind::Pin { parent: cell, at: number(at)? });
            }
            if let Some((at, held)) = rest.split_once('/')
                && let Some(bucket) = held.strip_prefix(PIN).and_then(|held| held.strip_prefix('.'))
            {
                // Not a bucket's number: no hold's key.
                let bucket = u16::from_str_radix(bucket, 16).ok()?;
                return Some(Kind::Hold { parent: cell, at: number(at)?, bucket });
            }
            if let Some((at, rest)) = rest.split_once('/')
                && let Some(rest) = rest.strip_prefix(EPHEMERAL).and_then(|r| r.strip_prefix('.'))
            {
                let (incarnation, child) = rest.split_once('.')?;
                let (tenant, child) = child.split_once('.')?;
                let child = CellId { tenant: number(tenant)?, id: number(child)? };
                let (at, incarnation) = (number(at)?, number(incarnation)?);
                return Some(Kind::Ephemeral { parent: cell, at, incarnation, child });
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
        if let Some(slot) = rest.strip_prefix("head/") {
            return Some(Kind::Head {
                cell,
                slot: number(slot).filter(|&slot| slot < MAX_HEAD_SLOTS)?,
            });
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    /// cladefs: the swaps of the head before this one (TASK-691): it is in slot `swaps % N`, N its
    /// bucket's count of slots, each swap writing the slot after its predecessor's, conditional on
    /// what was there. Its maker's head is version 0, then its copies, 1 to `N - 1`, its other
    /// slots ([`crate::head`]).
    pub swaps: u64,
    /// cladefs: whether this is a mark of a head failed as it was made, in a slot its maker left
    /// empty, no head ([`crate::head::fail`], TASK-691).
    pub failed: bool,
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
}

/// cladefs: what a guest's fsync waits for in a cell (doc-12, doc-21): its commit's level. The
/// sync family (sync, syncfs, unmount) waits for [`Fsync::Replicated`] in a replicated cell, and
/// for its upload in any other.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Fsync {
    /// In the daemon's memory, in order: survives what leaves the daemon alive.
    #[default]
    Ordered,
    /// On the writer's host's disk.
    Local,
    /// On f+1 members' disks (or uploaded): survives the writer's host.
    Replicated,
}

/// cladefs: the most f a cell is replicated with: 2f+1 = 7 copies.
pub const MAX_F: u8 = 3;

/// cladefs: what a replicated cell asks for, as its taking gives it ([`Head::replication`]): its
/// f, and what its guest's fsync waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wish {
    pub f: u8,
    pub fsync: Fsync,
}

impl Wish {
    /// A wish of `f` and `fsync`, unless `f` is past [`MAX_F`], or `fsync` waits for copies an
    /// f of 0 never makes.
    pub fn new(f: u8, fsync: Fsync) -> Option<Self> {
        (f <= MAX_F && !(f == 0 && fsync == Fsync::Replicated)).then_some(Self { f, fsync })
    }
}

/// cladefs: a replicated cell's replication (doc-21): what it asks for, and the configuration
/// its writer's commits are replicated in, once it has one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replication {
    pub wish: Wish,
    /// The active configuration: none until a writer installs one.
    pub configuration: Option<Configuration>,
}

/// cladefs: a configuration of a replicated cell (doc-21): its writer and 2f members, holding a
/// write-once log of the commits the head does not name yet. Made by a taking, or by its writer
/// replacing members, and active once the head names it. An f of 0, its writer alone, holds no
/// quorum: a taker of a cell whose head names it reads the bucket alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Member {
    /// `HOST:PORT`, as the hosts sharing the bucket name it (`CLADEFSD_PEERS`).
    pub address: String,
    pub incarnation: u64,
}

/// cladefs: a version a head names ([`Head::versions`], doc-18, doc-20): its ID, which the footer
/// of the segment making it named ([`NewVersion`]); the segment it is at and that segment's writer
/// `epoch` (board 6017); and whether forks of it may be of other tenants' cells (doc-7, Shared
/// versions).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
        Self { format: FORMAT, owner, made, ..Self::default() }
    }

    /// The head of `cell` that `bytes` encode, unless it is of another format than this one, newer
    /// or older, which its first bytes say ([`crate::Error::Format`]). cladefs: refused as corrupt
    /// unless its runs name every segment of `0..end` (board 8545): a bucket's object, untrusted
    /// until a MAC is checked, which the keyless collector checks none of; past here, the epoch of
    /// each segment it names is there ([`Head::segments`]).
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    pub fn decode(cell: CellId, bytes: &[u8]) -> Result<Self, crate::Error> {
        let format: u32 = decode(bytes.get(..4).unwrap_or(bytes))?;
        if format != FORMAT {
            return Err(crate::Error::Format { cell, format });
        }
        let head: Self = decode(bytes)?;
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

/// cladefs: a mark-only pass's marks object ([`marks`]): what it would have deleted, each object as
/// its listing found it, which readers warn of reaching, and the next such pass compares what it
/// finds doomed with (TASK-264).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marks {
    /// The epoch of the collector's lease the pass took.
    pub epoch: u64,
    /// Its grace period, in milliseconds by the backend's clock ([`crate::collector::Options`]):
    /// what it found doomed, a pass of no shorter one finds doomed too.
    pub grace: u64,
    /// The objects marked, in the order of their keys.
    pub marked: Vec<Marked>,
}

/// cladefs: a reference's bytes ([`reference`]): the format of the bucket's objects, first, as a
/// record's, and the fork's owner, its child's ([`Child::owner`], a clone's its restore's): unique
/// to the fork, so that every writer of it writes the same, a writable fork's host, its parent's
/// writers and its parent's deletion alike, and a create landed though its answer was lost is taken
/// for its own (TASK-395.3). What the collector keeps by is the key alone; nothing reads them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    pub format: u32,
    pub owner: u64,
}

impl Reference {
    /// The reference of the fork whose child's owner is `owner`.
    pub fn of(owner: u64) -> Self {
        Self { format: FORMAT, owner }
    }
}

/// cladefs: a mirror's marker's bytes ([`mirror`], TASK-633.8, board 6017): the format of the
/// bucket's objects alone; the checkpoint epoch is carried directly in `Origin`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mirror {
    pub format: u32,
}

impl Mirror {
    pub const NOW: Self = Self { format: FORMAT };
}

/// cladefs: a reference's mark's bytes ([`retired`]): the format of the bucket's objects alone, the
/// same whoever writes it. What a pass deletes by is its key alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retired {
    pub format: u32,
}

impl Retired {
    pub const NOW: Self = Self { format: FORMAT };
}

/// An object a mark-only pass found doomed, as the listing it went by found it: an object at its
/// key of another version is another object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marked {
    pub key: String,
    pub size: u64,
    pub e_tag: Option<String>,
    pub version: Option<String>,
    /// When it was written, by the backend's clock.
    pub modified: u64,
}

impl Marked {
    /// The object `meta` lists.
    pub fn of(meta: &crate::bucket::Meta) -> Self {
        Self {
            key: meta.key.clone(),
            size: meta.size,
            e_tag: meta.version.e_tag.clone(),
            version: meta.version.version.clone(),
            modified: meta.modified,
        }
    }

    /// Whether `meta` lists it: the object at its key, of its version.
    pub fn is(&self, meta: &crate::bucket::Meta) -> bool {
        self.key == meta.key
            && self.e_tag == meta.version.e_tag
            && self.version == meta.version.version
            && self.size == meta.size
    }
}

/// The footer of a compacted object's moves: where in the moves each extent the object held, live,
/// now is. cladefs: and the key of the object they are the moves of, which a reader checks against
/// theirs: a whole moves copied to another's key is no moves of it (TASK-411.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Moves {
    pub of: String,
    pub extents: Vec<(ExtentId, Location)>,
}

pub fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    bincode::serialize(value).expect("objects serialize")
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, crate::Error> {
    bincode::deserialize(bytes).map_err(|error| crate::Error::Corrupt(error.to_string()))
}

pub use extent_frame::{Frame, PART, TRAILER, crc};

/// How a writer encrypts what it frames: under its cell's DEK, the newest, and which it is
/// ([`crate::keys::Ring::encrypting`], TASK-411.3). Nothing is framed in the clear (TASK-411.28).
pub type EncryptionKey<'a> = (&'a extent_frame::Secret, extent_frame::KeyId);

/// The bytes of an object of `data` and `footer`, framed (`extent_frame`): the data's parts,
/// compressed, the footer, the table of where the parts are, then the [`TRAILER`]. cladefs:
/// encrypted as `encrypting` says (TASK-411.3).
pub fn with_footer<T: Serialize>(
    data: &[u8],
    footer: &T,
    encrypting: EncryptionKey<'_>,
) -> Vec<u8> {
    extent_frame::framed(data, &encode(footer), encrypting)
}

/// cladefs: the object of `extent`, stored on its own, of `data`: its footer names it, so that one
/// copied to another key, encrypted, is refused (TASK-411.3), encrypted as `encrypting` says.
pub fn own_extent(extent: ExtentId, data: &[u8], encrypting: EncryptionKey<'_>) -> Vec<u8> {
    with_footer(data, &extent, encrypting)
}

/// The data of a whole object [`with_footer`] made, checked against its CRCs, by a reader holding
/// `keys` (TASK-411.3): none of an object encrypted under a key not of them
/// ([`crate::Error::NoKey`]).
pub fn data_under(bytes: &[u8], keys: &dyn extent_frame::Keys) -> Result<Vec<u8>, crate::Error> {
    Ok(extent_frame::data(bytes, keys)?)
}

/// The footer of a whole object [`with_footer`] made, checked against its CRC.
pub fn footer<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, crate::Error> {
    footer_under(bytes, &extent_frame::NoKeys)
}

/// cladefs: the footer of a whole object, as [`footer`], by a reader holding `keys`: checked
/// against its MAC too, if it holds the object's key, and refused as `extent_frame` refuses it
/// (TASK-411.3).
pub fn footer_under<T: DeserializeOwned>(
    bytes: &[u8],
    keys: &dyn extent_frame::Keys,
) -> Result<T, crate::Error> {
    decode(extent_frame::footer(bytes, keys)?)
}

impl Segment {
    /// Its object, cladefs: encrypted as `encrypting` says (TASK-411.3).
    pub fn encode(&self, encrypting: EncryptionKey<'_>) -> Vec<u8> {
        with_footer(&self.data, &self.footer, encrypting)
    }

    /// The footer of a segment's object.
    pub fn footer(bytes: &[u8]) -> Result<Footer, crate::Error> {
        footer(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The moves of `key`, a cell's object's, by the pass of epoch 1.
    fn moves(key: &str) -> String {
        super::moves(key, 1).unwrap()
    }

    /// TASK-395.2: the moves of a moves are of what it is of, along the way, in its cell alone.
    #[test]
    fn moves_are_further_along_the_way_in_their_cell() {
        let cell = CellId { tenant: 0xa, id: 0xc };
        let sealed = segment(CellId { tenant: cell.tenant, id: 3 }, 7, 2);
        let (once, twice) = (moves(&sealed), moves(&moves(&sealed)));
        assert!(further(&sealed, &once) && further(&sealed, &twice) && further(&once, &twice));
        assert!(!further(&once, &sealed) && !further(&twice, &once) && !further(&sealed, &sealed));
        let other = segment(CellId { tenant: 0xa, id: 4 }, 7, 2);
        assert!(!further(&other, &once) && !further(&sealed, &moves(&other)));
        assert_eq!(moved_from(&once), Some(sealed.clone()));
        assert_eq!(moved_from(&twice), Some(once.clone()));
        assert_eq!(moved_from(&sealed), None);
    }

    /// TASK-572: how deep moves of moves are, each a level more, at the key the collector would
    /// make for the deepest it compacts within the [`MAX_KEY`] bytes buckets take, its hops within those
    /// a read follows.
    #[test]
    fn moves_are_as_deep_as_their_chains() {
        let sealed = segment(CellId { tenant: 0xa, id: 3 }, 7, 2);
        assert_eq!(moves_depth(&sealed), 0);
        let mut key = sealed;
        for depth in 1..=crate::collector::MOVES_DEPTH {
            key = super::moves(&key, u64::MAX).unwrap();
            assert_eq!(moves_depth(&key), depth);
        }
        assert!(key.len() <= MAX_KEY, "{} bytes: {key}", key.len());
        assert!(crate::collector::MOVES_DEPTH < crate::moves::MOVES);
    }

    /// TASK-407.1: two passes' moves of one object are at keys of their own, under the object's
    /// moves' prefix, which no other object's moves are under, nor its moves' moves.
    #[test]
    fn passes_moves_of_one_object_are_at_keys_of_their_own() {
        let cell = CellId { tenant: 0xa, id: 0xc };
        let sealed = segment(cell, 7, 2);
        let (first, second) =
            (super::moves(&sealed, 1).unwrap(), super::moves(&sealed, 2).unwrap());
        assert_ne!(first, second);
        let prefix = moves_of(&sealed).unwrap();
        assert!(first.starts_with(&prefix) && second.starts_with(&prefix));
        assert_eq!(
            (moved_from(&first), moved_from(&second)),
            (Some(sealed.clone()), Some(sealed.clone()))
        );
        let next = segment(cell, 8, 2);
        assert!(!super::moves(&next, 1).unwrap().starts_with(&prefix));
        assert!(!moves(&first).starts_with(&prefix));
        let of = "segments/0000000000002.0000000000007";
        assert_eq!(kind(&second), Kind::Moves { cell, of, pass: 2 });
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
        assert_eq!(kind(&head(cell, 3)), Kind::Head { cell, slot: 3 });
        // cladefs: a slot past the head's is no head's (TASK-691).
        let past = format!("{}{}", heads(cell), Padded(MAX_HEAD_SLOTS));
        assert_eq!(kind(&past), Kind::Unknown);
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
        assert_eq!(kind(&marks(cell, 0x2a)), Kind::Marks { cell, epoch: 0x2a });
        assert_eq!(kind(&format!("{}gc/marks/2a", cell.prefix())), Kind::Unknown);
        assert_eq!(kind(&format!("{}gc/other", cell.prefix())), Kind::Unknown);
        let from = segment(cell, 7, 2);
        let of = "segments/0000000000002.0000000000007";
        assert_eq!(kind(&moves(&from)), Kind::Moves { cell, of, pass: 1 });
        let moved = format!("moves/{of}/0000000000001");
        assert_eq!(kind(&moves(&moves(&from))), Kind::Moves { cell, of: &moved, pass: 1 });
        assert_eq!(kind(&format!("{}moves/", cell.prefix())), Kind::Unknown);
        assert_eq!(kind(&format!("{}moves/{of}", cell.prefix())), Kind::Unknown);
        let child = CellId { tenant: 1, id: 5 };
        assert_eq!(
            kind(&reference(cell, 7, child)),
            Kind::Reference { parent: cell, at: 7, child }
        );
        assert_eq!(kind(&retired(cell, 7, child)), Kind::Retired { parent: cell, at: 7, child });
        assert_eq!(retired_of(&reference(cell, 7, child)), Some(retired(cell, 7, child)));
        assert_eq!(retired_of(&retired(cell, 7, child)), None);
        assert!(!retired(cell, 7, child).starts_with(&references(cell)));
        // cladefs: a mirror's pin, among its version's references, and its marker (TASK-633.8).
        assert_eq!(kind(&pin(cell, 7)), Kind::Pin { parent: cell, at: 7 });
        assert_eq!(kind(&hold(cell, 7, 3)), Kind::Hold { parent: cell, at: 7, bucket: 3 });
        assert_eq!(owner(&hold(cell, 7, 3)), Some(cell));
        assert!(pin(cell, 7).starts_with(&origin(cell, 7)));
        assert_eq!(retired_of(&pin(cell, 7)), None);
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
        // entries' prefix, never under it (boards 8517, 8519).
        let index = ephemeral_index(0x2a, cell, 7, child);
        assert!(index.starts_with(&ephemeral_indexes(ephemeral_bucket(cell), 0x2a)));
        assert_eq!(ephemeral_indexed(&index), Some((0x2a, cell, 7, child)));
        assert_eq!(kind(&index), Kind::EphemeralIndex { incarnation: 0x2a });
        assert_eq!(owner(&index), None);
        let theirs = CellId { tenant: 1, id: 3 << 48 | 5 };
        let there = ephemeral_index(0x2a, theirs, 7, child);
        assert_eq!(
            there,
            "ephemeral/0003/000000000001a/0000000000001.000r000000005.0000000000007.0000000000001.0000000000005"
        );
        assert_eq!((routed_to(&there), routed_to(&ephemeral_hosts(3))), (Some(3), Some(3)));
        assert_eq!(routed_to(&index), None, "a named cell's stays in the bucket asked");
        let forged = there.replacen("/0003/", "/0002/", 1);
        assert_eq!((ephemeral_indexed(&forged), kind(&forged)), (None, Kind::Unknown));
        let tombstone = decommissioned(3, 0x2a);
        assert_eq!(tombstone, "ephemeral/0003/000000000001a.decommissioned");
        assert!(!tombstone.starts_with(&ephemeral_indexes(3, 0x2a)));
        assert!(tombstone.starts_with(&ephemeral_hosts(3)));
        assert_eq!(decommissioned_of(&tombstone), Some((3, 0x2a)));
        assert_eq!(kind(&tombstone), Kind::Decommissioned { incarnation: 0x2a });
        assert_eq!((owner(&tombstone), routed_to(&tombstone)), (None, Some(3)));
        for malformed in [
            "ephemeral/3/000000000001a.decommissioned",
            "ephemeral/+003/000000000001a.decommissioned",
            "ephemeral/0003/1a.decommissioned",
            "ephemeral/0003/000000000001a.decommissione",
        ] {
            assert_eq!(kind(malformed), Kind::Unknown, "{malformed}");
        }
        assert!(mirror(cell, 7).starts_with(&mirrors(cell)));
        assert_eq!((owner(&pin(cell, 7)), owner(&mirror(cell, 7))), (Some(cell), Some(cell)));
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
        assert_eq!(kind(&format!("moves/{from}")), Kind::Unknown);
        assert_eq!(cell_of_prefix(&cell.prefix()), Some(cell));
        assert_eq!(cell_of_prefix(&head(cell, 0)), None);
        assert_eq!(cell_of_prefix(""), None);
    }

    /// TASK-394, TASK-395.2: every kind of a cell's objects is the cell's, its moves however many
    /// times moved, its collector's lease and marks, and the references under it, of forks of its
    /// cells to other cells; object-ID blocks and keys of no kind are no cell's.
    #[test]
    fn owners() {
        let cell = CellId { tenant: 0xa, id: 0xc };
        let child = CellId { tenant: 1, id: 5 };
        let keys = [
            segment(cell, 7, 2),
            extent(cell, 2, 4, ExtentId::new(3, 9)),
            checkpoint(cell, 7, 2),
            head(cell, 0),
            moves(&segment(cell, 7, 2)),
            moves(&moves(&extent(cell, 2, 4, ExtentId::new(3, 9)))),
            collector_lease(cell),
            marks(cell, 1),
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
}
