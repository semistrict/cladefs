// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: what made a cell, which is what it is ([`Made`]), and what that allows, asked of its
//! head ([`Head`], doc-7, Layout; TASK-160.14, TASK-515): whether a writer takes it, whether the
//! collector removes it once dead, how its fork's reference retires.
//!
//! Every change of a head goes through [`crate::lifecycle`]'s one transition, or a writer's swap.

use crate::layout::{Head, Origin};
use crate::{Error, rules};
use serde::{Deserialize, Serialize};

/// What made a cell, and so what it is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Made {
    /// A cell of no parent, which its first writer made ([`crate::head::take`]).
    Root,
    /// A cell a deletion found no head of, a fork's child before its head or no cell
    /// ([`crate::lifecycle::delete`], [`crate::lifecycle::delete_fork`]): deleted from the first.
    #[default]
    Own,
    /// A fork of a version, naming the origin the version names as its own, until the fork is made
    /// ([`crate::lifecycle::fork_version`]): until then it holds nothing, and nothing relies on it.
    /// The collector gives it up once its cell's head names the version no more
    /// ([`crate::lifecycle::abandon`]).
    Forking { origin: Origin, version: u64 },
    /// cladefs: a fork of a version, made: what [`Made::Forking`] becomes, given its head by its
    /// own host alone, and its reference, written before it was made, by that host alone
    /// (TASK-455). So, of an ID an allocator gave it, deleted, it goes with its cell
    /// ([`Head::removable`]), and its reference retires unmarked ([`Head::retires_unmarked`]).
    Clone(Origin),
}

impl Made {
    /// The cell it was forked from, and the segment of that cell's at whose end, if any.
    pub fn origin(self) -> Option<Origin> {
        match self {
            Self::Root | Self::Own => None,
            Self::Clone(origin) | Self::Forking { origin, .. } => Some(origin),
        }
    }
}

impl Head {
    /// The cell it was forked from, and the segment of that cell's at whose end, if any.
    pub fn origin(&self) -> Option<Origin> {
        self.made.origin()
    }

    /// The version it is being forked from, until the fork is made ([`Made::Forking`]).
    pub fn forking(&self) -> Option<u64> {
        match self.made {
            Made::Forking { version, .. } => Some(version),
            _ => None,
        }
    }

    /// Whether it is the head of the child of the fork whose child's owner is `owner`. Any other
    /// is another cell's, whose ID the fork was given (TASK-79.82).
    pub fn is_child_of(&self, owner: u64) -> bool {
        self.made != Made::Root && self.owner == owner
    }

    /// Whether a writer may take the cell, and if not why: not once it is deleted; nor a fork of
    /// a version not yet made, which holds nothing and may be given up on.
    pub fn writable(&self) -> Result<(), Error> {
        if self.deleted() {
            return Err(Error::Deleted);
        }
        match self.made {
            Made::Forking { .. } => Err(Error::NoCell),
            _ => Ok(()),
        }
    }

    /// cladefs: whether the collector may remove it, with its cell, the cell dead: deleted, a
    /// root's or a clone's made ([`Made::Clone`]), heads their makers alone make, once, of IDs an
    /// allocator gave (doc-7, Object IDs; TASK-455). Any other stays for good: a deletion's of a
    /// cell it found none of ([`Made::Own`]), which may be a fork's child whose head is yet to
    /// land, a dead host's; and a clone's never made, whose host may yet write its reference.
    pub fn removable(&self) -> bool {
        self.deleted() && matches!(self.made, Made::Root | Made::Clone(_))
    }

    /// cladefs: whether the reference of its fork, its cell deleted, is retired unmarked: a
    /// clone's made by its host alone, its one writer, which wrote it before it made the clone
    /// (TASK-455). A mark is for a writer of it that may write it after the retirement: a clone
    /// resumed by another ([`Head::resumed`]), whose host, slow, may (rule `resumes_marked`), and
    /// one never made, whose host may yet.
    pub fn retires_unmarked(&self) -> bool {
        matches!(self.made, Made::Clone(_)) && !(self.resumed && rules().resumes_marked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{FORMAT, encode, test_cell};

    /// A head says its format first: one of another format, newer or older, is refused, naming
    /// it, whatever follows, which this format doesn't read (TASK-141.2, TASK-165.3).
    #[test]
    fn heads_of_other_formats_are_refused() {
        let head = Head::of(Made::Own, 7);
        let bytes = encode(&head);
        assert_eq!(&bytes[..4], &FORMAT.to_le_bytes());
        assert_eq!(Head::decode(test_cell(3), &bytes), Ok(head));
        let mut newer = (FORMAT + 1).to_le_bytes().to_vec();
        newer.extend_from_slice(b"what a later format holds");
        assert_eq!(
            Head::decode(test_cell(3), &newer),
            Err(Error::Format { cell: test_cell(3), format: FORMAT + 1 })
        );
        // Format 1's and 2's records, whose footers and checkpoints this format reads wrong; 7's,
        // whose versions say nothing of being shared (TASK-395.7); 8's, whose collector's lease
        // and moves are at the bucket's root, its cells' collection under their own prefixes
        // reading neither (TASK-395.2); and 9's, whose records and cells keep no keys, their
        // objects framed in FORMAT 1 of extent_frame (TASK-411.3); and
        // 10's, whose keys name no epochs, and whose cells have leases, not heads (TASK-407);
        // and 11's, whose clones made say they are forks a recorder may record (TASK-455); and
        // 12's, whose forks of versions say nothing of being resumed (TASK-455's review); and 13's,
        // whose heads name no versions, nor footers the versions cut at their ends (TASK-476); and
        // 14's, whose keys' numbers are in hexadecimal (TASK-504); 15's, whose cells name maps
        // of cells (TASK-508.1); 16's, whose records name versions (TASK-508.2); 17's, whose
        // records are flags, of writable forks too (TASK-508.3); and 18's, whose cells have a
        // record and an object beside their heads (TASK-515); and 19's, whose buckets have no
        // number, their cells' IDs unique in them alone (TASK-633.9).
        assert_eq!(FORMAT, 20);
        for format in [1u32, 2, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19] {
            let mut older = bytes.clone();
            older[..4].copy_from_slice(&format.to_le_bytes());
            assert_eq!(
                Head::decode(test_cell(3), &older),
                Err(Error::Format { cell: test_cell(3), format })
            );
        }
        assert!(matches!(Head::decode(test_cell(3), &bytes[..3]), Err(Error::Corrupt(_))));
    }

    /// Every kind of head, deleted or not, resumed or not, decodes back to itself.
    #[test]
    fn heads_decode_to_what_they_encode() {
        let origin = Origin { parent: test_cell(1), at: 7 };
        let made =
            [Made::Root, Made::Own, Made::Forking { origin, version: 0xbe }, Made::Clone(origin)];
        for made in made {
            for (epoch, resumed) in
                [(0, false), (Head::FINAL, false), (3, true), (Head::FINAL, true)]
            {
                let head = Head { epoch, resumed, ..Head::of(made, 9) };
                assert_eq!(Head::decode(test_cell(2), &encode(&head)), Ok(head));
            }
        }
    }
}
