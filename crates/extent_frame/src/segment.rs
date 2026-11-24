// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A segment's footer (doc-7, Segments), the same in a cell's closed segments and in object
//! storage, which hold the same bytes: a closed local segment is uploaded as it is; and the chain
//! of one writer's footers ([`Chain`]), which stamps every footer its segments close with, a
//! cell's and a writer's alike, and admits those another closed for it.

use crate::Corrupt;
use base32_id::{Padded, Short};
use fprint::TypeFingerprint;
use serde::{Deserialize, Serialize};
use storage_device::ExtentId;

/// Where an extent's bytes are: a range of the data of an object.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Location {
    pub key: String,
    pub offset: u64,
    pub len: u64,
}

/// cladefs: a tenant's cell: its tenant, and its ID, unique across buckets, which its bucket's
/// allocator gives under the bucket's number (doc-20, doc-23): what its keys are under, and what an
/// extent's ID names as the cell that wrote it. Tenants are IDs alone; what they stand for, their
/// callers keep.
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
pub struct CellId {
    pub tenant: u64,
    pub id: u64,
}

impl CellId {
    /// The prefix of the keys of the cell's objects: `tenants/<tenant>/cells/<id>/`.
    pub fn prefix(self) -> String {
        format!("tenants/{}/cells/{}/", Padded(self.tenant), Padded(self.id))
    }
}

/// cladefs: a cell as operators read and write it: `TENANT/CELL`, each in base 32
/// (`cladefs-gc --cell`, TASK-395.4).
impl std::fmt::Display for CellId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", Short(self.tenant), Short(self.id))
    }
}

impl std::str::FromStr for CellId {
    type Err = String;

    /// The cell `TENANT/CELL` names, each in base 32, as it is displayed.
    fn from_str(text: &str) -> Result<Self, String> {
        let parsed = match text.split('/').collect::<Vec<_>>()[..] {
            [tenant, id] => base32_id::parse(tenant).zip(base32_id::parse(id)),
            _ => None,
        };
        let parsed = parsed.map(|(tenant, id)| Self { tenant, id });
        parsed.ok_or_else(|| format!("no cell, TENANT/CELL in base 32: {text}"))
    }
}

/// What a segment holds after its data: its place in its writer's chain, and the commits it makes,
/// its [`Contents`]. A segment with no data is a footer alone: commits that only free extents or
/// set the root, or make versions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Footer {
    pub cell: CellId,
    pub segment: u64,
    /// The epoch of its writer's taking, which its key names too: a segment is the cell's only
    /// once the cell's head names it, by its number and epoch (doc-7, The head; TASK-407).
    pub epoch: u64,
    /// What its commits made.
    pub contents: Contents,
}

impl Footer {
    pub fn encode(&self) -> Vec<u8> {
        bincode::serialize(self).expect("footers serialize")
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Corrupt> {
        bincode::deserialize(bytes).map_err(|err| Corrupt(format!("a footer: {err}")))
    }

    /// The extents its commits sealed that are stored on their own, the cell's, and where each
    /// is: the object its writer uploaded before it, at the key naming the writer's epoch and it
    /// ([`extent_key`]), which tells the collector whose it is.
    pub fn own(&self) -> impl Iterator<Item = (ExtentId, Location)> + '_ {
        self.contents.own_at(self.cell, self.epoch, self.segment)
    }
}

/// What a segment's commits made, which its footer holds, stamped with its place by its writer's
/// chain ([`Chain::close`]): its commits' tags, unique to each commit (a cell's segment names the
/// last of its commits, [`tag`]); the extents they sealed in its data, at an offset, of a length;
/// the cell's extents they sealed that are stored on their own, by their sequence numbers, of a
/// length, at the writer's keys ([`Footer::own`]): an inherited extent a cell uploads goes in its
/// data, as a key of the extents a cell uploads names that cell alone (TASK-150.1); the extents
/// they free, the cell's, or inherited; the root, if they set one; the cell's next sequence
/// number as of them, at least; and the cell's
/// capacity, the most bytes its extents may hold, if they set it: a resize is a commit's, ordered
/// with the others, so that the cell has it wherever it is taken, its forks inherit it, and a
/// version keeps it as it was (TASK-165.3). cladefs: and the runs in its data of the commit it ends
/// inside of, if it does, which the next segment goes on with ([`Continued`], TASK-92.41).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub struct Contents {
    pub commits: Vec<u64>,
    pub extents: Vec<(ExtentId, u64, u64)>,
    pub own: Vec<(u64, u64)>,
    pub frees: Vec<ExtentId>,
    pub root: Option<Vec<u8>>,
    pub next_sequence: u64,
    pub capacity: Option<u64>,
    pub continued: Option<Continued>,
    /// cladefs: the versions made at its end, which the acknowledgement naming it names in the
    /// head ([`NewVersion`], TASK-476).
    pub versions: Vec<NewVersion>,
}

/// cladefs: a version made at a segment's end (doc-18, doc-20): a name for its cell's state as of
/// the segment, of no record, kept for as long as the head names it, which the acknowledgement
/// naming the segment does, in the same swap. Making it is local: the commit closing the segment
/// in the cell's log, nothing in the bucket, which the upload names later. Its ID is random,
/// unique to its making, as an owner is: the segment's number may change, as a WAL is put after the
/// cell's end (its `Wal::rebase`), its ID never. The commit making it seals nothing, its
/// cell quiescent.
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
pub struct NewVersion {
    pub id: u64,
    /// The versions of its cell its maker asked to keep, itself among them, the newest: once
    /// the head names it, the others are dropped, in the same swap. None, all.
    pub keep_last: Option<u32>,
}

/// cladefs: the runs a commit sealed in a segment's data, past the commits the segment ends, which
/// the segments after it go on with: a commit spanning segments (TASK-92.41, the user's decision
/// of 2026-10-23). A guest writing more than a segment between two commits fills segments that
/// hold no commit; the commit after seals each extent where it was written, in whichever of them
/// it is one run of, and each of those segments names its runs of the commit, its tag `commit`,
/// as this; the segment the commit ends in names the commit with the rest of it, its frees, its
/// root, its extents of their own, as any segment names a commit it ends. Footers name runs in
/// their own segments alone. A reader holds what a segment continues until the segment that ends
/// the commit, then seals it with the rest ([`Contents::ends`]); a segment after that neither goes
/// on with it nor ends it, as a fence a later writer put there, leaves it unsealed: the commit,
/// never ended, never was. Before, the commit copied every such extent out, into a file of its own
/// each (TASK-79.2, TASK-310).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub struct Continued {
    pub commit: u64,
    pub extents: Vec<(ExtentId, u64, u64)>,
}

impl Contents {
    /// cladefs: the runs sealed in its segment's data: those of the commits it ends, then those of
    /// the commit it goes on with, if any ([`Continued`]).
    pub fn runs(&self) -> impl Iterator<Item = &(ExtentId, u64, u64)> + '_ {
        let continued = self.continued.iter().flat_map(|continued| &continued.extents);
        self.extents.iter().chain(continued)
    }

    /// cladefs: whether a segment of these contents, following one that went on with commit
    /// `commit` ([`Continued`]), ends it: it names a commit of `commit`'s writer, `commit` or a
    /// later one, as the segment a cell's commit ends in names its last; one that names none, a
    /// fence, ends it not, nor does one going on with it.
    pub fn ends(&self, commit: u64) -> bool {
        self.commits.last().is_some_and(|&last| {
            writer_of(last) == writer_of(commit) && epoch_of(last) >= epoch_of(commit)
        })
    }

    /// cladefs: whether a segment of these contents, following one that went on with commit
    /// `commit`, goes on with it too, ending no commit.
    pub fn continues(&self, commit: u64) -> bool {
        self.commits.is_empty()
            && self.continued.as_ref().is_some_and(|continued| continued.commit == commit)
    }

    /// The extents sealed on their own, `cell`'s, and where each is, in segment `segment` of the
    /// writer of epoch `epoch`: the object that writer uploaded before it ([`extent_key`]).
    pub fn own_at(
        &self,
        cell: CellId,
        epoch: u64,
        segment: u64,
    ) -> impl Iterator<Item = (ExtentId, Location)> + '_ {
        self.own.iter().map(move |&(sequence, len)| {
            let extent = ExtentId { cell: cell.id, sequence };
            (extent, Location { key: extent_key(cell, epoch, segment, extent), offset: 0, len })
        })
    }
}

/// One writer's footers, in the order its segments close (doc-7, The head): numbered on without
/// gaps from the cell's end as its head named it; all of the writer's epoch, their extents
/// of their own at its keys; the next sequence number each names never below its floor nor the one
/// before's. Whatever closes a writer's segments, a cell
/// or the writer, stamps their footers with the writer's chain, and the writer admits each footer
/// only if its chain would have stamped it so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chain {
    cell: CellId,
    epoch: u64,
    /// The number of the next segment.
    next_segment: u64,
    /// The next sequence number the next footer names, at least.
    next_sequence: u64,
}

impl Chain {
    /// The chain of the writer of epoch `epoch` of `cell`, whose next segment is numbered
    /// `next_segment`; the footers it stamps name `next_sequence`, the writer's floor, as the
    /// cell's next sequence number at least.
    pub const fn new(cell: CellId, epoch: u64, next_segment: u64, next_sequence: u64) -> Self {
        Self { cell, epoch, next_segment, next_sequence }
    }

    pub fn cell(&self) -> CellId {
        self.cell
    }

    /// The epoch of its writer's taking.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The number of the next segment.
    pub fn next_segment(&self) -> u64 {
        self.next_segment
    }

    /// The cell's next sequence number, as of the last footer: what the writer's guest mints
    /// from.
    pub fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    /// The tag its writer gives the cell's commit `epoch` ([`tag`]).
    pub fn tag(&self, epoch: u64) -> u64 {
        tag(self.epoch, epoch)
    }

    /// The footer of the next segment, which holds `contents`, stamped with the chain's place,
    /// which names its extents of their own at the writer's keys. The chain goes on after it.
    pub fn close(&mut self, contents: Contents) -> Footer {
        let next_sequence = contents.next_sequence.max(self.next_sequence);
        let footer = Footer {
            cell: self.cell,
            segment: self.next_segment,
            epoch: self.epoch,
            contents: Contents { next_sequence, ..contents },
        };
        self.follow(&footer);
        footer
    }

    /// The chain after `footer`, which another closed for it, if it is the next footer it would
    /// have stamped: of its cell, number and epoch.
    pub fn admit(&self, footer: &Footer) -> Result<Self, Corrupt> {
        let wanted = (self.cell, self.next_segment, self.epoch);
        if (footer.cell, footer.segment, footer.epoch) != wanted {
            return Err(Corrupt(format!(
                "a segment of cell {}, number {}, epoch {}, not {wanted:?}",
                footer.cell, footer.segment, footer.epoch
            )));
        }
        let mut chain = *self;
        chain.follow(footer);
        Ok(chain)
    }

    /// Goes on after `footer`, its next segment's.
    fn follow(&mut self, footer: &Footer) {
        self.next_segment = footer.segment + 1;
        self.next_sequence = self.next_sequence.max(footer.contents.next_sequence);
    }
}

/// Commits' tags: the writer's epoch above this bit, the cell's epoch below, so that no two
/// writers' commits share one.
pub const TAG_SHIFT: u32 = 40;

/// The tag of the cell's commit `epoch` that the writer of epoch `writer` uploads.
pub fn tag(writer: u64, epoch: u64) -> u64 {
    (writer << TAG_SHIFT) | epoch
}

/// The cell's epoch a commit's tag names.
pub fn epoch_of(tag: u64) -> u64 {
    tag & ((1 << TAG_SHIFT) - 1)
}

/// The epoch of the writer that uploads the commit a tag names.
pub fn writer_of(tag: u64) -> u64 {
    tag >> TAG_SHIFT
}

/// The key of the object of an extent stored on its own, uploaded by the writer of epoch
/// `epoch` before its segment `segment`, whose footer names it. cladefs: the segment's number tells
/// the collector whether the object is ahead of its segment, which may yet land, or a landed one's,
/// which no state naming it frees it: what the cell's next sequence number can't tell, as its
/// extents are committed in no order of their sequence numbers (TASK-79.3). Under the prefix of
/// `cell`, its cell's (TASK-364).
pub fn extent_key(cell: CellId, epoch: u64, segment: u64, extent: ExtentId) -> String {
    format!(
        "{}extents/{}.{}.{}",
        cell.prefix(),
        Padded(epoch),
        Padded(segment),
        Padded(extent.sequence)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cell of the tests' cells.
    const CELL: CellId = CellId { tenant: 1, id: 2 };

    /// The cell `id` of `CELL`'s tenant.
    const fn cell(id: u64) -> CellId {
        CellId { tenant: CELL.tenant, id }
    }

    /// A footer decodes as it was encoded: a cell's segments and the bucket's objects hold them.
    #[test]
    fn footers_decode_as_encoded() {
        let footer = Footer {
            cell: cell(2),
            segment: 9,
            epoch: 4,
            contents: Contents {
                commits: vec![tag(4, 11), tag(4, 12)],
                extents: vec![(ExtentId { cell: 2, sequence: 5 }, 0, 4096)],
                own: vec![(6, 512)],
                frees: vec![ExtentId { cell: 1, sequence: 1 }],
                root: Some(b"root".to_vec()),
                next_sequence: 13,
                capacity: Some(1 << 30),
                continued: Some(Continued {
                    commit: tag(4, 13),
                    extents: vec![(ExtentId { cell: 2, sequence: 14 }, 4096, 8192)],
                }),
                versions: vec![NewVersion { id: 0xbe, keep_last: Some(3) }],
            },
        };
        let bytes = footer.encode();
        assert_eq!(Footer::decode(&bytes).unwrap(), footer);
        assert!(Footer::decode(&bytes[..bytes.len() - 1]).is_err());
    }

    /// cladefs: a commit a segment goes on with is ended by a segment naming it, or a later commit
    /// of its writer's, as the segment a cell's commit ends in names its last; gone on with by one
    /// naming no commit and going on with it; and neither by a fence, nor by another writer's
    /// segment, nor by one going on with another commit (TASK-92.41). A segment's runs are its
    /// commits', then its continued commit's.
    #[test]
    fn continued_commits_end_in_their_writers_segments() {
        let commit = tag(4, 13);
        let extent = |sequence| ExtentId { cell: 2, sequence };
        let continuing = |commit| Contents {
            continued: Some(Continued { commit, extents: vec![(extent(2), 0, 8)] }),
            ..Contents::default()
        };
        let naming = |last| Contents { commits: vec![last], ..Contents::default() };
        assert!(naming(commit).ends(commit) && naming(tag(4, 20)).ends(commit));
        assert!(!naming(tag(4, 12)).ends(commit) && !naming(tag(5, 13)).ends(commit));
        assert!(!Contents::default().ends(commit) && !continuing(commit).ends(commit));
        assert!(continuing(commit).continues(commit));
        assert!(!continuing(tag(4, 14)).continues(commit));
        assert!(!Contents::default().continues(commit));
        let both = Contents { commits: vec![tag(4, 12)], ..continuing(commit) };
        assert!(!both.continues(commit), "a segment ending a commit goes on with none before it");
        let runs = Contents { extents: vec![(extent(1), 8, 8)], ..continuing(commit) };
        assert_eq!(runs.runs().collect::<Vec<_>>(), [&(extent(1), 8, 8), &(extent(2), 0, 8)]);
    }

    /// A copy of a chain admits the footer the chain closed, and goes on from it as the chain did;
    /// it refuses one of another cell, number or epoch, and goes on from none.
    #[test]
    fn chains_admit_only_what_they_would_close() {
        let start = Chain::new(cell(2), 4, 9, 100);
        let mut closing = start;
        let footer = closing.close(Contents { own: vec![(7, 512)], ..Contents::default() });
        assert_eq!(start.admit(&footer), Ok(closing));
        // A footer of another place than the next the chain closes: refused, saying both.
        let misplaced =
            |place: &str| format!("a segment of cell {place}, not {:?}", (cell(2), 9, 4));
        for (wrong, refused) in [
            (Footer { cell: cell(3), ..footer.clone() }, misplaced("1/3, number 9, epoch 4")),
            (
                Footer { cell: CellId { tenant: 5, ..cell(2) }, ..footer.clone() },
                misplaced("5/2, number 9, epoch 4"),
            ),
            (Footer { segment: 10, ..footer.clone() }, misplaced("1/2, number 10, epoch 4")),
            (Footer { epoch: 5, ..footer }, misplaced("1/2, number 9, epoch 5")),
        ] {
            assert_eq!(start.admit(&wrong), Err(Corrupt(refused)), "{wrong:?}");
        }
        assert_eq!(start, Chain::new(cell(2), 4, 9, 100));
    }
}
