// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A segment builder ([`SegmentBuilder`]): commits a writer makes into one segment of its own
//! (doc-7, Layout), rather than take a segment a cell closed. Closed on the writer's chain, it is
//! the writer's next segment, framed as the cell frames its own ([`Unframed::frame`]), which the
//! writer takes as it takes a cell's ([`Writer::add_segment`](crate::Writer::add_segment)).

use crate::layout::{self, CellId, Chain, Contents, Footer, NewVersion, Segment};
use crate::writer::Objects;
use std::collections::{BTreeMap, HashMap, HashSet};
use storage_device::ExtentId;

/// A commit: the extents it seals, which it minted, stored in its segment or, `own`, by their
/// sequence numbers, on their own; the extents it frees; and the root and the cell's capacity,
/// if it sets them. `tag` names it in footers.
#[derive(Clone, Debug, Default)]
pub struct Commit {
    pub tag: u64,
    pub extents: Vec<(ExtentId, Vec<u8>)>,
    pub own: Vec<(u64, Vec<u8>)>,
    /// Pre-framed own extents `(sequence, logical_len, framed_bytes)` copied directly without
    /// recompression/re-encryption (used by fast WAL takeover recovery, doc-29).
    pub own_preframed: Vec<(u64, u64, Vec<u8>)>,
    pub frees: Vec<ExtentId>,
    pub root: Option<Vec<u8>>,
    pub capacity: Option<u64>,
    /// Minimum next sequence number recorded by this commit (preserves allocation floor across
    /// commits that freed or skipped sequence numbers, e.g. fast WAL takeover replay, doc-29).
    pub next_sequence: u64,
}

impl Commit {
    /// Whether it changes nothing: it seals and frees no extent, and sets no root nor capacity.
    pub fn is_empty(&self) -> bool {
        self.extents.is_empty()
            && self.own.is_empty()
            && self.own_preframed.is_empty()
            && self.frees.is_empty()
            && self.root.is_none()
            && self.capacity.is_none()
    }
}

/// What one segment's commits make, by the lengths of the extents they seal: the contents of its
/// footer, once closed on its writer's chain; the extents in its data one after another, from where
/// its data begins. cladefs: as a cell's segment does, its footer seals what its commits sealed
/// that the cell holds as of its last, and frees what they freed that a commit held: one sealed by
/// a commit and freed by a later is a free alone, which spends it; one a commit seals and frees
/// itself no commit held, and is neither (TASK-160.15, TASK-165.3).
#[derive(Default)]
pub struct Plan {
    /// Its commits' tags, root, capacity, and the next sequence number as of them: its footer's
    /// contents but what [`Plan::contents`] makes of the rest.
    contents: Contents,
    /// The extents its commits seal, in its data, at an offset, of a length, and on their own, by
    /// their sequence numbers, of a length; and those they free: each with the commit, by its
    /// index, that sealed or freed it.
    extents: Vec<((ExtentId, u64, u64), usize)>,
    own: Vec<((u64, u64), usize)>,
    frees: Vec<(ExtentId, usize)>,
    /// How long its data is: where the next extent sealed in it goes.
    data: u64,
}

impl Plan {
    /// Seals `extent`, of `len` bytes, in its data, after those before it. The cell's next
    /// sequence number is past it.
    pub fn seal(&mut self, extent: ExtentId, len: u64) {
        self.extents.push(((extent, self.data, len), self.committing()));
        self.data += len;
        self.sealed(extent.sequence);
    }

    /// Seals the extent of the cell whose sequence number is `sequence`, of `len` bytes, on its
    /// own. The cell's next sequence number is past it.
    pub fn seal_own(&mut self, sequence: u64, len: u64) {
        self.own.push(((sequence, len), self.committing()));
        self.sealed(sequence);
    }

    /// The index of the commit being made.
    fn committing(&self) -> usize {
        self.contents.commits.len()
    }

    /// An extent of `sequence` is sealed.
    fn sealed(&mut self, sequence: u64) {
        self.contents.next_sequence = self.contents.next_sequence.max(sequence + 1);
    }

    /// Commits what it seals, as commit `tag`, which frees `frees`, and sets `root` and the
    /// cell's `capacity`, if given: after the commits before it.
    pub fn commit(
        &mut self,
        tag: u64,
        frees: Vec<ExtentId>,
        root: Option<Vec<u8>>,
        capacity: Option<u64>,
    ) {
        let commit = self.committing();
        self.frees.extend(frees.into_iter().map(|extent| (extent, commit)));
        if root.is_some() {
            self.contents.root = root;
        }
        if capacity.is_some() {
            self.contents.capacity = capacity;
        }
        self.contents.commits.push(tag);
    }

    /// Whether it holds no commit.
    fn is_empty(&self) -> bool {
        self.contents.commits.is_empty()
    }

    /// Its footer's contents, as `cell`'s: what its commits sealed that none of them freed, and
    /// what they freed that a commit before held.
    fn contents(self, cell: CellId) -> Contents {
        let Self { mut contents, extents, own, frees, .. } = self;
        let in_data = extents.iter().map(|&((extent, ..), by)| (extent, by));
        let on_own = own.iter().map(|&((sequence, _), by)| (ExtentId::new(cell.id, sequence), by));
        let sealed: HashMap<ExtentId, usize> = in_data.chain(on_own).collect();
        let mut freed = HashSet::new();
        for (extent, by) in frees {
            match sealed.get(&extent) {
                Some(&at) => {
                    freed.insert(extent);
                    if at < by {
                        contents.frees.push(extent);
                    }
                }
                None => contents.frees.push(extent),
            }
        }
        let held = |extent: &ExtentId| !freed.contains(extent);
        contents.extents = extents
            .into_iter()
            .map(|(sealed, _)| sealed)
            .filter(|(extent, ..)| held(extent))
            .collect();
        contents.own = (own.into_iter())
            .map(|(sealed, _)| sealed)
            .filter(|&(sequence, _)| held(&ExtentId::new(cell.id, sequence)))
            .collect();
        contents
    }

    /// Closes it on `chain`, with the versions `versions` cut at its end (TASK-476).
    fn close_ending(self, chain: &mut Chain, versions: Vec<NewVersion>) -> Footer {
        chain.close(Contents { versions, ..self.contents(chain.cell()) })
    }
}

/// Commits for one segment, in order: what they make ([`Plan`]), its data, and that of the
/// extents they seal on their own.
#[derive(Default)]
pub struct SegmentBuilder {
    plan: Plan,
    data: Vec<u8>,
    /// Those sealed on their own, by their sequence numbers.
    own: BTreeMap<u64, Vec<u8>>,
    /// Pre-framed own extents, keyed by sequence number, uploaded as-is (doc-29).
    own_preframed: BTreeMap<u64, Vec<u8>>,
}

impl From<Commit> for SegmentBuilder {
    /// A segment of `commit` alone.
    fn from(commit: Commit) -> Self {
        let mut batch = Self::default();
        batch.commit(commit);
        batch
    }
}

impl SegmentBuilder {
    /// Adds `commit`, after those before it.
    pub fn commit(&mut self, commit: Commit) {
        for (extent, bytes) in commit.extents {
            self.plan.seal(extent, bytes.len() as u64);
            self.data.extend_from_slice(&bytes);
        }
        for (sequence, bytes) in commit.own {
            self.plan.seal_own(sequence, bytes.len() as u64);
            self.own.insert(sequence, bytes);
        }
        for (sequence, logical_len, framed) in commit.own_preframed {
            self.plan.seal_own(sequence, logical_len);
            self.own_preframed.insert(sequence, framed);
        }
        self.plan.contents.next_sequence =
            self.plan.contents.next_sequence.max(commit.next_sequence);
        self.plan.commit(commit.tag, commit.frees, commit.root, commit.capacity);
    }

    /// Whether it holds no commit.
    pub fn is_empty(&self) -> bool {
        self.plan.is_empty()
    }

    /// Closes it on `chain`, its writer's, as the writer's next segment, which the chain goes on
    /// after: its footer, stamped, its data yet to be framed.
    pub fn close(self, chain: &mut Chain) -> Unframed {
        self.close_with_versions(chain, Vec::new())
    }

    /// cladefs: closes it on `chain`, as [`SegmentBuilder::close`] does, with the version `cut`
    /// cut at its end: a name for the cell's state there, which the acknowledgement naming the
    /// segment names in the cell's head (doc-18, TASK-476).
    pub fn cut(self, cut: NewVersion, chain: &mut Chain) -> Unframed {
        self.close_with_versions(chain, vec![cut])
    }

    /// Closes it on `chain` with `versions` cut at its end.
    pub fn close_with_versions(self, chain: &mut Chain, versions: Vec<NewVersion>) -> Unframed {
        let footer = self.plan.close_ending(chain, versions);
        Unframed {
            footer,
            data: Data { segment: self.data, own: self.own, own_preframed: self.own_preframed },
        }
    }
}

/// A segment builder closed, its segment's footer stamped, its data yet to be framed.
pub struct Unframed {
    pub footer: Footer,
    pub data: Data,
}

impl Unframed {
    /// Its objects, framed: the segment's, and its extents of their own's, at their keys. cladefs:
    /// encrypted as `encrypting` says, its writer's DEK (TASK-411.3).
    pub fn frame(self, encrypting: impl layout::KeyArg) -> Objects {
        self.data.frame(&self.footer, encrypting.as_key())
    }
}

/// A closed segment's data, and that of its extents stored on their own, by their sequence
/// numbers, unframed: framed with the footer that stamps it ([`Data::frame`]), the one its writer's
/// chain stamped, or, a takeover putting it after the cell's end, the next writer's
/// ([`Wal::rebase`](crate::wal::Wal::rebase)), which keeps its extents of their own at that
/// writer's keys.
#[derive(Clone, Debug, Default)]
pub struct Data {
    segment: Vec<u8>,
    own: BTreeMap<u64, Vec<u8>>,
    own_preframed: BTreeMap<u64, Vec<u8>>,
}

impl Data {
    /// The objects of the segment `footer` stamps, holding this data: the segment's, and its
    /// extents of their own's, at the keys `footer` names them at. cladefs: encrypted as
    /// `encrypting` says (TASK-411.3).
    pub fn frame(&self, footer: &Footer, encrypting: impl layout::KeyArg) -> Objects {
        let encrypting = encrypting.as_key();
        let own = (footer.own())
            .map(|(extent, location)| {
                let framed = match self.own_preframed.get(&extent.sequence) {
                    Some(framed) => framed.clone(),
                    None => {
                        let data = self.own.get(&extent.sequence).expect("sealed on its own");
                        layout::own_extent(extent, data, encrypting)
                    }
                };
                (location.key.clone(), framed.into())
            })
            .collect();
        let data_len = self.segment.len() as u64;
        let segment = Segment { data: self.segment.clone(), footer: footer.clone() };
        Objects {
            footer: footer.clone(),
            segment: segment.encode(encrypting).into(),
            own,
            data_len,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Footer;
    use crate::layout::test_cell;

    /// The next sequence number the footer of `commit`'s segment, closed on `chain`, names.
    fn next_sequence(commit: Commit, chain: &mut Chain) -> u64 {
        let objects = SegmentBuilder::from(commit)
            .close(chain)
            .frame(crate::testing::encrypting(test_cell(1)));
        layout::Segment::footer(&objects.segment.read().unwrap()).unwrap().contents.next_sequence
    }

    /// The sequence numbers a writer mints from are past every extent its commits seal, in its
    /// segments and on their own: the footers its chain stamps say so, and so does the chain.
    #[test]
    fn writers_mint_past_what_their_commits_seal() {
        let first = 5 << crate::EPOCH_SHIFT;
        let mut chain = Chain::new(test_cell(1), 5, 1, first);
        let extents = vec![(ExtentId::new(1, first + 3), b"in".to_vec())];
        assert_eq!(
            next_sequence(Commit { tag: 1, extents, ..Commit::default() }, &mut chain),
            first + 4
        );
        assert_eq!(chain.next_sequence(), first + 4);
        let own = vec![(first + 7, b"own".to_vec())];
        assert_eq!(
            next_sequence(Commit { tag: 2, own, ..Commit::default() }, &mut chain),
            first + 8
        );
        // One minted below adds nothing.
        let extents = vec![(ExtentId::new(1, first + 1), b"below".to_vec())];
        assert_eq!(
            next_sequence(Commit { tag: 3, extents, ..Commit::default() }, &mut chain),
            first + 8
        );
        assert_eq!(chain.next_sequence(), first + 8);
    }

    /// A batch's segment holds its commits' extents in its data, in order, those of their own in
    /// objects of their own at the writer's keys, its frees, its last root and its last capacity;
    /// a fork is at its end.
    #[test]
    fn batches_frame_their_commits() {
        let mut chain = Chain::new(test_cell(1), 2, 0, 0);
        let (small, large, freed) = (ExtentId::new(1, 3), ExtentId::new(1, 4), ExtentId::new(1, 1));
        let mut batch = SegmentBuilder::default();
        assert!(batch.is_empty());
        let extents = vec![(small, b"small".to_vec())];
        let (root, capacity) = (Some(b"one".to_vec()), Some(1 << 20));
        batch.commit(Commit { tag: 7, extents, root, capacity, ..Commit::default() });
        let own = vec![(large.sequence, b"large".to_vec())];
        let frees = vec![freed];
        let (root, capacity) = (Some(b"two".to_vec()), Some(2 << 20));
        batch.commit(Commit { tag: 8, own, frees, root, capacity, ..Commit::default() });
        batch.commit(Commit { tag: 9, ..Commit::default() });
        assert!(!batch.is_empty());
        let objects = batch.close(&mut chain).frame(crate::testing::encrypting(test_cell(1)));
        let segment = objects.segment.read().unwrap();
        let footer: Footer = layout::Segment::footer(&segment).unwrap();
        assert_eq!(footer, objects.footer);
        assert_eq!((footer.segment, footer.epoch), (0, 2));
        assert_eq!(footer.contents.commits, [7, 8, 9]);
        assert_eq!(footer.contents.extents, [(small, 0, 5)]);
        let key = layout::extent(layout::TEST_CELL, 2, footer.segment, large);
        assert_eq!(
            footer.own().collect::<Vec<_>>(),
            [(large, layout::Location { key: key.clone(), offset: 0, len: 5 })]
        );
        let contents = footer.contents;
        assert_eq!(
            (contents.frees, contents.root, contents.capacity),
            (vec![freed], Some(b"two".to_vec()), Some(2 << 20))
        );
        let keys = crate::testing::ring([test_cell(1)]);
        let data = |bytes: &[u8]| layout::data_under(bytes, &keys.of(1)).unwrap();
        assert_eq!(data(&segment), b"small");
        let [(own_key, own)] = &objects.own[..] else {
            panic!("{} objects of their own", objects.own.len())
        };
        let own = own.read().unwrap();
        assert_eq!((own_key, data(&own)), (&key, b"large".to_vec()));
        assert_eq!(chain.next_segment(), 1);
    }

    /// TASK-165.3: as a cell's segment's footer does, a batch's seals what its commits sealed that
    /// none of them freed, and frees what a commit before the freeing one held: an extent sealed
    /// and later freed in it, in its data or on its own, is a free alone; one a commit seals and
    /// frees itself is neither; an inherited extent of the same sequence number as one sealed on
    /// its own is freed as it is.
    #[test]
    fn batches_free_what_a_commit_before_held() {
        let mut chain = Chain::new(test_cell(1), 2, 0, 0);
        let (small, large, other) = (ExtentId::new(1, 3), ExtentId::new(1, 4), ExtentId::new(1, 5));
        let (written, inherited, kept) =
            (ExtentId::new(1, 6), ExtentId::new(9, 4), ExtentId::new(1, 7));
        let mut batch = SegmentBuilder::default();
        let extents = vec![(small, b"small".to_vec()), (kept, b"kept".to_vec())];
        let own = vec![(large.sequence, b"large".to_vec()), (other.sequence, b"other".to_vec())];
        batch.commit(Commit { tag: 7, extents, own, ..Commit::default() });
        let extents = vec![(written, b"written".to_vec())];
        let frees = vec![small, large, written, inherited];
        batch.commit(Commit { tag: 8, extents, frees, ..Commit::default() });
        let objects = batch.close(&mut chain).frame(crate::testing::encrypting(test_cell(1)));
        let footer = objects.footer;
        assert_eq!(footer.contents.extents, [(kept, 5, 4)]);
        let key = layout::extent(layout::TEST_CELL, 2, 0, other);
        assert_eq!(
            footer.own().collect::<Vec<_>>(),
            [(other, layout::Location { key: key.clone(), offset: 0, len: 5 })]
        );
        assert_eq!(footer.contents.frees, [small, large, inherited]);
        let own: Vec<_> = objects.own.iter().map(|(key, _)| key.clone()).collect();
        assert_eq!(own, [key]);
    }
}
