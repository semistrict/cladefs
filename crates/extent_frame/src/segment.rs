// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A segment's footer (doc-7, Segments), the same in a store's closed segments and in object
//! storage, which hold the same bytes: a closed local segment is uploaded as it is; and the chain
//! of one writer's footers ([`Chain`]), which stamps every footer its segments close with, a
//! store's and a writer's alike, and admits those another closed for it.

use crate::Corrupt;
use base32_id::{Padded, Short};
use serde::{Deserialize, Serialize};
use storage_device::ExtentId;

/// Where an extent's bytes are: a range of the data of an object.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Location {
    pub key: String,
    pub offset: u64,
    pub len: u64,
}

/// cladefs: a tenant's cell, its unit of placement: one host at a time writes all the branches in it,
/// under its one taking, and keeps them under its prefix in the bucket (TASK-364). Tenants and
/// cells are IDs alone; what they stand for, their callers keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Cell {
    pub tenant: u64,
    pub id: u64,
}

impl Cell {
    /// The prefix of the keys of the cell's objects: `tenants/<tenant>/cells/<id>/`.
    pub fn prefix(self) -> String {
        format!("tenants/{}/cells/{}/", Padded(self.tenant), Padded(self.id))
    }
}

/// cladefs: a cell as operators read and write it: `TENANT/CELL`, each in base 32, as a branch's
/// first two parts (`cladefs-gc --cell`, TASK-395.4).
impl std::fmt::Display for Cell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", Short(self.tenant), Short(self.id))
    }
}

impl std::str::FromStr for Cell {
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

/// cladefs: where a branch is: its cell, and its ID, which no other branch its extents are inherited
/// from or by has: an extent's ID names its branch by the ID alone (TASK-364).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Branch {
    pub cell: Cell,
    pub id: u64,
}

impl Branch {
    /// The branch `id` of cell `cell` of this one's tenant: where a fork of this one goes, a cell
    /// of its own (TASK-364).
    pub fn in_cell(self, cell: u64, id: u64) -> Self {
        Self { cell: Cell { tenant: self.cell.tenant, id: cell }, id }
    }
}

/// A branch as operators read and write it: `TENANT/CELL/ID`, each in base 32.
impl std::fmt::Display for Branch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.cell, Short(self.id))
    }
}

impl std::str::FromStr for Branch {
    type Err = String;

    /// The branch `TENANT/CELL/ID` names, each in base 32, as it is displayed.
    fn from_str(text: &str) -> Result<Self, String> {
        let id = base32_id::parse;
        let parsed = match text.split('/').collect::<Vec<_>>()[..] {
            [tenant, cell, branch] => (id(tenant), id(cell), id(branch)),
            _ => (None, None, None),
        };
        match parsed {
            (Some(tenant), Some(cell), Some(id)) => {
                Ok(Self { cell: Cell { tenant, id: cell }, id })
            }
            _ => Err(format!("no branch, TENANT/CELL/ID in base 32: {text}")),
        }
    }
}

/// cladefs: a fork made at a segment's end: its child's branch, the child's owner, which every
/// record of the child names, whoever writes it (TASK-79.82, TASK-153.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Child {
    pub branch: Branch,
    pub owner: u64,
}

/// What a segment holds after its data: its place in its writer's chain, and the commits it makes,
/// its [`Contents`]. A segment with no data is a footer alone: commits that only free extents or
/// set the root, or forks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Footer {
    pub branch: Branch,
    pub segment: u64,
    /// The epoch of its writer's taking, which its key names too: a segment is the branch's only
    /// once the branch's head names it, by its number and epoch (doc-7, The head; TASK-407).
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

    /// The extents its commits sealed that are stored on their own, the branch's, and where each
    /// is: the object its writer uploaded before it, at the key naming the writer's epoch and it
    /// ([`extent_key`]), which tells the collector whose it is.
    pub fn own(&self) -> impl Iterator<Item = (ExtentId, Location)> + '_ {
        self.contents.own_at(self.branch, self.epoch, self.segment)
    }
}

/// What a segment's commits made, which its footer holds, stamped with its place by its writer's
/// chain ([`Chain::close`]): its commits' tags, unique to each commit (a store's segment names the
/// last of its commits, [`tag`]); the extents they sealed in its data, at an offset, of a length;
/// the branch's extents they sealed that are stored on their own, by their sequence numbers, of a
/// length, at the writer's keys ([`Footer::own`]): an inherited extent a branch uploads goes in its
/// data, as a key of the extents a branch uploads names that branch alone (TASK-150.1); the extents
/// they free, the branch's, or inherited; the root, if they set one; the branch's next sequence
/// number as of them, at least; the forks made at its end, by their children; and the branch's
/// capacity, the most bytes its extents may hold, if they set it: a resize is a commit's, ordered
/// with the others, so that the branch has it wherever it is taken, its forks inherit it, and a
/// point keeps it as it was (TASK-165.3). cladefs: and the runs in its data of the commit it ends
/// inside of, if it does, which the next segment goes on with ([`Continued`], TASK-92.41).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contents {
    pub commits: Vec<u64>,
    pub extents: Vec<(ExtentId, u64, u64)>,
    pub own: Vec<(u64, u64)>,
    pub frees: Vec<ExtentId>,
    pub root: Option<Vec<u8>>,
    pub next_sequence: u64,
    pub forks: Vec<Child>,
    pub capacity: Option<u64>,
    pub continued: Option<Continued>,
    /// cladefs: the versions made at its end, which the acknowledgement naming it names in the
    /// head ([`NewVersion`], TASK-476).
    pub versions: Vec<NewVersion>,
}

/// cladefs: a version made at a segment's end (doc-18, doc-20): a name for its cell's state as of
/// the segment, of no record, kept for as long as the head names it, which the acknowledgement
/// naming the segment does, in the same swap. Making it is local: the commit closing the segment
/// in the store's log, nothing in the bucket, which the upload names later. Its ID is random,
/// unique to its making, as an owner is: the segment's number may change, as a WAL is put after the
/// branch's end (a store's `Wal::rebase`), its ID never. The commit making it seals nothing, its
/// store quiescent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    /// later one, as the segment a store's commit ends in names its last; one that names none, a
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

    /// The extents sealed on their own, `branch`'s, and where each is, in segment `segment` of the
    /// writer of epoch `epoch`: the object that writer uploaded before it ([`extent_key`]).
    pub fn own_at(
        &self,
        branch: Branch,
        epoch: u64,
        segment: u64,
    ) -> impl Iterator<Item = (ExtentId, Location)> + '_ {
        self.own.iter().map(move |&(sequence, len)| {
            let extent = ExtentId { branch: branch.id, sequence };
            (
                extent,
                Location { key: extent_key(branch.cell, epoch, segment, extent), offset: 0, len },
            )
        })
    }
}

/// One writer's footers, in the order its segments close (doc-7, The head): numbered on without
/// gaps from the branch's end as its head named it; all of the writer's epoch, their extents
/// of their own at its keys; the next sequence number each names never below its floor nor the one
/// before's. Whatever closes a writer's segments, a store
/// or the writer, stamps their footers with the writer's chain, and the writer admits each footer
/// only if its chain would have stamped it so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chain {
    branch: Branch,
    epoch: u64,
    /// The number of the next segment.
    next_segment: u64,
    /// The next sequence number the next footer names, at least.
    next_sequence: u64,
}

impl Chain {
    /// The chain of the writer of epoch `epoch` of `branch`, whose next segment is numbered
    /// `next_segment`; the footers it stamps name `next_sequence`, the writer's floor, as the
    /// branch's next sequence number at least.
    pub const fn new(branch: Branch, epoch: u64, next_segment: u64, next_sequence: u64) -> Self {
        Self { branch, epoch, next_segment, next_sequence }
    }

    pub fn branch(&self) -> Branch {
        self.branch
    }

    /// The epoch of its writer's taking.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The number of the next segment.
    pub fn next_segment(&self) -> u64 {
        self.next_segment
    }

    /// The branch's next sequence number, as of the last footer: what the writer's guest mints
    /// from.
    pub fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    /// The tag its writer gives the store's commit `epoch` ([`tag`]).
    pub fn tag(&self, epoch: u64) -> u64 {
        tag(self.epoch, epoch)
    }

    /// The footer of the next segment, which holds `contents`, stamped with the chain's place,
    /// which names its extents of their own at the writer's keys. The chain goes on after it.
    pub fn close(&mut self, contents: Contents) -> Footer {
        let next_sequence = contents.next_sequence.max(self.next_sequence);
        let footer = Footer {
            branch: self.branch,
            segment: self.next_segment,
            epoch: self.epoch,
            contents: Contents { next_sequence, ..contents },
        };
        self.follow(&footer);
        footer
    }

    /// The chain after `footer`, which another closed for it, if it is the next footer it would
    /// have stamped: of its branch, number and epoch.
    pub fn admit(&self, footer: &Footer) -> Result<Self, Corrupt> {
        let wanted = (self.branch, self.next_segment, self.epoch);
        if (footer.branch, footer.segment, footer.epoch) != wanted {
            return Err(Corrupt(format!(
                "a segment of branch {}, number {}, epoch {}, not {wanted:?}",
                footer.branch, footer.segment, footer.epoch
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

/// Commits' tags: the writer's epoch above this bit, the store's epoch below, so that no two
/// writers' commits share one.
pub const TAG_SHIFT: u32 = 40;

/// The tag of the store's commit `epoch` that the writer of epoch `writer` uploads.
pub fn tag(writer: u64, epoch: u64) -> u64 {
    (writer << TAG_SHIFT) | epoch
}

/// The store's epoch a commit's tag names.
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
/// which no state naming it frees it: what the branch's next sequence number can't tell, as its
/// extents are committed in no order of their sequence numbers (TASK-79.3). Under the prefix of
/// `cell`, its branch's (TASK-364).
pub fn extent_key(cell: Cell, epoch: u64, segment: u64, extent: ExtentId) -> String {
    format!(
        "{}extents/{}/{}.{}.{}",
        cell.prefix(),
        Padded(extent.branch),
        Padded(epoch),
        Padded(segment),
        Padded(extent.sequence)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cell of the tests' branches.
    const CELL: Cell = Cell { tenant: 1, id: 2 };

    /// Branch `id` of `CELL`.
    const fn branch(id: u64) -> Branch {
        Branch { cell: CELL, id }
    }

    /// A footer decodes as it was encoded: a store's segments and the bucket's objects hold them.
    #[test]
    fn footers_decode_as_encoded() {
        let footer = Footer {
            branch: branch(2),
            segment: 9,
            epoch: 4,
            contents: Contents {
                commits: vec![tag(4, 11), tag(4, 12)],
                extents: vec![(ExtentId { branch: 2, sequence: 5 }, 0, 4096)],
                own: vec![(6, 512)],
                frees: vec![ExtentId { branch: 1, sequence: 1 }],
                root: Some(b"root".to_vec()),
                next_sequence: 13,
                forks: vec![
                    Child { branch: branch(3), owner: 0x33 },
                    Child {
                        branch: Branch { cell: Cell { tenant: 5, id: 6 }, id: 4 },
                        owner: 0x44,
                    },
                ],
                capacity: Some(1 << 30),
                continued: Some(Continued {
                    commit: tag(4, 13),
                    extents: vec![(ExtentId { branch: 2, sequence: 14 }, 4096, 8192)],
                }),
                versions: vec![NewVersion { id: 0xbe, keep_last: Some(3) }],
            },
        };
        let bytes = footer.encode();
        assert_eq!(Footer::decode(&bytes).unwrap(), footer);
        assert!(Footer::decode(&bytes[..bytes.len() - 1]).is_err());
    }

    /// cladefs: a commit a segment goes on with is ended by a segment naming it, or a later commit of
    /// its writer's, as the segment a store's commit ends in names its last; gone on with by one
    /// naming no commit and going on with it; and neither by a fence, nor by another writer's
    /// segment, nor by one going on with another commit (TASK-92.41). A segment's runs are its
    /// commits', then its continued commit's.
    #[test]
    fn continued_commits_end_in_their_writers_segments() {
        let commit = tag(4, 13);
        let extent = |sequence| ExtentId { branch: 2, sequence };
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

    /// cladefs: a branch reads as it displays, `TENANT/CELL/ID` in base 32; anything else is no
    /// branch.
    #[test]
    fn branches_parse_as_they_display() {
        let branch = Branch { cell: Cell { tenant: 10, id: 32 * 11 + 22 }, id: 33 };
        assert_eq!(branch.to_string(), "a/bp/11");
        assert_eq!("a/bp/11".parse(), Ok(branch));
        assert_eq!("000000000000a/bp/0000000000011".parse(), Ok(branch));
        for wrong in ["11", "a/bp", "a/bp/11/4", "a/bp/xu", "a/bp/B", "a/bp/", ""] {
            let refused = format!("no branch, TENANT/CELL/ID in base 32: {wrong}");
            assert_eq!(wrong.parse::<Branch>(), Err(refused));
        }
    }

    /// A tag holds its writer's epoch above `TAG_SHIFT` and the store's below, each recovered whole.
    #[test]
    fn tags_hold_both_epochs() {
        assert_eq!(tag(3, 5), 0x0300_0000_0005);
        let last = (1 << TAG_SHIFT) - 1;
        assert_eq!(tag(1, last), 0x01ff_ffff_ffff);
        assert_eq!((writer_of(tag(3, 5)), epoch_of(tag(3, 5))), (3, 5));
        let highest = u64::MAX >> TAG_SHIFT;
        assert_eq!((writer_of(tag(highest, last)), epoch_of(tag(highest, last))), (highest, last));
        assert_eq!((writer_of(tag(7, 0)), epoch_of(tag(7, 0))), (7, 0));
    }

    /// A chain numbers the segments it closes on from where it starts, of its writer's epoch; the next sequence number each names is the chain's floor at
    /// least, and never below the one before's.
    #[test]
    fn chains_number_what_they_close() {
        let own = ExtentId { branch: 2, sequence: 7 };
        let mut chain = Chain::new(branch(2), 4, 9, 100);
        let contents = Contents {
            commits: vec![tag(4, 1)],
            extents: vec![(ExtentId { branch: 2, sequence: 6 }, 0, 4096)],
            own: vec![(own.sequence, 512)],
            next_sequence: 50,
            ..Contents::default()
        };
        assert_eq!(
            chain.close(contents),
            Footer {
                branch: branch(2),
                segment: 9,
                epoch: 4,
                contents: Contents {
                    commits: vec![tag(4, 1)],
                    extents: vec![(ExtentId { branch: 2, sequence: 6 }, 0, 4096)],
                    own: vec![(own.sequence, 512)],
                    next_sequence: 100,
                    ..Contents::default()
                },
            }
        );
        let second = chain.close(Contents { next_sequence: 200, ..Contents::default() });
        let place = |footer: &Footer| (footer.segment, footer.contents.next_sequence);
        assert_eq!(place(&second), (10, 200));
        let third = chain.close(Contents { next_sequence: 150, ..Contents::default() });
        assert_eq!(place(&third), (11, 200));
        let place = (chain.branch(), chain.epoch(), chain.next_segment());
        assert_eq!((place, chain.next_sequence()), ((branch(2), 4, 12), 200));
        assert_eq!(chain.tag(5), tag(4, 5));
    }

    /// A copy of a chain admits the footer the chain closed, and goes on from it as the chain did;
    /// it refuses one of another branch, number or epoch, and goes on from none.
    #[test]
    fn chains_admit_only_what_they_would_close() {
        let start = Chain::new(branch(2), 4, 9, 100);
        let mut closing = start;
        let footer = closing.close(Contents { own: vec![(7, 512)], ..Contents::default() });
        assert_eq!(start.admit(&footer), Ok(closing));
        // A footer of another place than the next the chain closes: refused, saying both.
        let misplaced =
            |place: &str| format!("a segment of branch {place}, not {:?}", (branch(2), 9, 4));
        for (wrong, refused) in [
            (Footer { branch: branch(3), ..footer.clone() }, misplaced("1/2/3, number 9, epoch 4")),
            (
                Footer {
                    branch: Branch { cell: Cell { tenant: 1, id: 3 }, ..branch(2) },
                    ..footer.clone()
                },
                misplaced("1/3/2, number 9, epoch 4"),
            ),
            (Footer { segment: 10, ..footer.clone() }, misplaced("1/2/2, number 10, epoch 4")),
            (Footer { epoch: 5, ..footer }, misplaced("1/2/2, number 9, epoch 5")),
        ] {
            assert_eq!(start.admit(&wrong), Err(Corrupt(refused)), "{wrong:?}");
        }
        assert_eq!(start, Chain::new(branch(2), 4, 9, 100));
    }

    /// A footer's extents of their own are the branch's, at the keys of its writer's epoch and its
    /// number: wherever it was closed, no other.
    #[test]
    fn footers_name_their_own_extents_at_their_writers_keys() {
        let mut chain = Chain::new(branch(2), 4, 9, 100);
        let footer = chain.close(Contents { own: vec![(7, 512), (5, 8)], ..Contents::default() });
        let (seven, five) =
            (ExtentId { branch: 2, sequence: 7 }, ExtentId { branch: 2, sequence: 5 });
        assert_eq!(
            footer.own().collect::<Vec<_>>(),
            [
                (seven, Location { key: extent_key(CELL, 4, 9, seven), offset: 0, len: 512 }),
                (five, Location { key: extent_key(CELL, 4, 9, five), offset: 0, len: 8 }),
            ]
        );
    }

    /// The keys of extents stored on their own: under their cell's prefix, by tenant and cell; then
    /// by branch, the writer's epoch, the segment that names them, and sequence, in fixed-width
    /// base 32, so that a listing is in that order.
    #[test]
    fn extent_keys_order_by_cell_branch_epoch_segment_and_sequence() {
        let cell = Cell { tenant: 10, id: 12 };
        let key = extent_key(cell, 7, 42, ExtentId { branch: 2, sequence: 31 });
        let expected = "tenants/000000000000a/cells/000000000000c/extents/\
                        0000000000002/0000000000007.000000000001a.000000000000z";
        assert_eq!(key, expected);
    }
}
