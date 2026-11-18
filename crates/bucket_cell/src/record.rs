// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a cell's record: what made it, which is what it is ([`Kind`]), whose it is, whether
//! it was deleted (doc-7, Layout; TASK-160.14), and, of a version, whether other tenants may fork it
//! ([`Record::shared`], TASK-395.7); cladefs: and its DEKs, wrapped by its cell's KEK, if its cell
//! is encrypted ([`Record::deks`], doc-16, TASK-411.3).
//!
//! What a record allows is asked of it here, not where it is read; and every change of one goes
//! through [`crate::lifecycle`]'s one transition.

use crate::keys::{Dek, InheritedDek};
use crate::layout::{CellId, FORMAT, Origin, decode};
use crate::{Error, rules};
use serde::{Deserialize, Serialize};

/// What made a cell, and so what it is: the kinds of record there are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    /// A cell of no parent, which its maker created ([`crate::lifecycle::create`]).
    Root,
    /// A cell a deletion found no record of, a fork's child before its record or no cell
    /// ([`crate::lifecycle::delete`], [`crate::lifecycle::delete_fork`]): deleted from the first.
    Own,
    /// A fork of a version, naming the origin the version names as its own, until the fork is made
    /// ([`crate::lifecycle::fork_version`]): until then it holds nothing, and nothing relies on it.
    /// The collector gives it up once its cell's head names the version no more
    /// ([`crate::lifecycle::abandon`]).
    Forking { origin: Origin, version: u64 },
    /// cladefs: a fork of a version, made: what [`Kind::Forking`] becomes, given its record by its
    /// own host alone, and its reference, written before it was made, by that host alone
    /// (TASK-455). So, of an ID an allocator gave it, deleted, it goes with its cell
    /// ([`Record::removable`]), and its reference retires unmarked ([`Record::retires_unmarked`]).
    Clone(Origin),
}

impl Kind {
    /// A record of this kind and format, live, of `owner`.
    pub fn of(self, owner: u64) -> Record {
        Record {
            format: FORMAT,
            owner,
            kind: self,
            deleted: false,
            resumed: false,
            deks: Vec::new(),
            inherited: Vec::new(),
        }
    }

    /// The cell it was forked from, and the segment of that cell's at whose end, if any.
    pub fn origin(self) -> Option<Origin> {
        match self {
            Self::Root | Self::Own => None,
            Self::Clone(origin) | Self::Forking { origin, .. } => Some(origin),
        }
    }
}

/// A cell's record: of its kind, which says where it came from, and whether it was deleted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The format of the cell's objects ([`FORMAT`]), first, so that a reader of any format
    /// reads it before the rest, which it can read only if it is of its own ([`Record::decode`],
    /// TASK-141.2, TASK-165.3).
    pub format: u32,
    /// The cell's owner: a number its maker chose at random, unique to that making, which every
    /// record of the cell names, whoever writes it ([`crate::layout::Child::owner`]). So no two
    /// makers of one cell ID write the same bytes, and a create that finds a record there tells its
    /// own, landed though its answer was lost, from another's (TASK-79.82, TASK-153.9).
    pub owner: u64,
    pub kind: Kind,
    /// A deleted cell's record stays; but a given root's and clone's, which go with their cell
    /// once it is dead ([`Record::removable`]).
    pub deleted: bool,
    /// cladefs: of a fork of a version, being made or made, whether another than the host that
    /// began it resumed it ([`crate::lifecycle::resume_version_fork`]): a store opening its child, its
    /// host dead or slow, or a fork of the version finding the record its own (TASK-455's
    /// review). Said
    /// with a condition, while the fork is being made, before the resume writes its reference:
    /// so a clone made says it if anyone but its host may write its reference after, whose
    /// retirement is then marked ([`Record::retires_unmarked`]).
    pub resumed: bool,
    /// cladefs: the cell's DEK, each generation of it, wrapped by its cell's KEK, oldest first:
    /// none before its first writer took it (TASK-411.3);
    /// a writer's ring of a record with none is refused (TASK-411.28). Given by its writer
    /// ([`crate::keys::with_dek`]), whoever made the record.
    pub deks: Vec<Dek>,
    /// cladefs: the DEKs of every ancestor of another cell whose objects the cell reads, each
    /// generation, rewrapped under its own cell's KEK (doc-16, Forks and shared versions;
    /// TASK-411.4): given with its first DEK, in one conditional update, so that a record with
    /// DEKs holds all it inherits from other cells, its readers needing no other cell's KEK, nor
    /// another tenant's KMS. By its fork's host, as it makes the fork
    /// ([`crate::keys::give_inherited`]), or its first writer ([`crate::keys::with_dek`]).
    pub inherited: Vec<InheritedDek>,
}

impl Record {
    /// The record of `cell` that `bytes` encode, unless it is of another format than this one,
    /// newer or older, which its first bytes say ([`Error::Format`]).
    pub fn decode(cell: CellId, bytes: &[u8]) -> Result<Self, Error> {
        let format: u32 = decode(bytes.get(..4).unwrap_or(bytes))?;
        if format != FORMAT {
            return Err(Error::Format { cell, format });
        }
        decode(bytes)
    }

    /// The cell it was forked from, and the segment of that cell's at whose end, if any.
    pub fn origin(&self) -> Option<Origin> {
        self.kind.origin()
    }

    /// The version it is being forked from, until the fork is made ([`Kind::Forking`]).
    pub fn forking(&self) -> Option<u64> {
        match self.kind {
            Kind::Forking { version, .. } => Some(version),
            _ => None,
        }
    }

    /// Whether it is the record of the child of the fork whose child's owner is `owner`. Any other
    /// is another cell's, whose ID the fork was given (TASK-79.82).
    pub fn is_child_of(&self, owner: u64) -> bool {
        self.kind != Kind::Root && self.owner == owner
    }

    /// Whether a writer may take the cell, and if not why: not once it is deleted; nor a fork of
    /// a version not yet made, which holds nothing and may be given up on.
    pub fn writable(&self) -> Result<(), Error> {
        if self.deleted {
            return Err(Error::Deleted);
        }
        match self.kind {
            Kind::Forking { .. } => Err(Error::NoCell),
            _ => Ok(()),
        }
    }

    /// cladefs: whether the collector may remove it, with its cell, the cell dead: deleted, a
    /// root's or a clone's made ([`Kind::Clone`]), records their makers alone make, once, of IDs
    /// an allocator gave (doc-7, Object IDs; TASK-455). Any other stays for good: a deletion's of
    /// a cell it found none of ([`Kind::Own`]), which may be a fork's child whose record is yet to
    /// land, a dead host's; and a clone's never made, whose host may yet write its reference.
    pub fn removable(&self) -> bool {
        self.deleted && matches!(self.kind, Kind::Root | Kind::Clone(_))
    }

    /// cladefs: whether the reference of its fork, its cell deleted, is retired unmarked: a
    /// clone's made by its host alone, its one writer, which wrote it before it made the clone
    /// (TASK-455). A mark is for a writer of it that may write it after the retirement: a clone
    /// resumed by another ([`Record::resumed`]), whose host, slow, may (rule `resumes_marked`), and
    /// one never made, whose host may yet.
    pub fn retires_unmarked(&self) -> bool {
        matches!(self.kind, Kind::Clone(_)) && !(self.resumed && rules().resumes_marked)
    }

    /// cladefs: it, of a fork of a version being made, said resumed ([`Record::resumed`]).
    pub fn resumed(self) -> Self {
        Self { resumed: true, ..self }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::encode;
    use crate::layout::test_cell;

    /// A record says its format first: one of another format, newer or older, is refused, naming
    /// it, whatever follows, which this format doesn't read (TASK-141.2, TASK-165.3).
    #[test]
    fn records_of_other_formats_are_refused() {
        let record = Kind::Own.of(7);
        let bytes = encode(&record);
        assert_eq!(&bytes[..4], &FORMAT.to_le_bytes());
        assert_eq!(Record::decode(test_cell(3), &bytes), Ok(record));
        let mut newer = (FORMAT + 1).to_le_bytes().to_vec();
        newer.extend_from_slice(b"what a later format holds");
        assert_eq!(
            Record::decode(test_cell(3), &newer),
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
        // of cells (TASK-508.1); 16's, whose records name versions (TASK-508.2); and 17's, whose
        // records are flags, of writable forks too (TASK-508.3).
        assert_eq!(FORMAT, 18);
        for format in [1u32, 2, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17] {
            let mut older = bytes.clone();
            older[..4].copy_from_slice(&format.to_le_bytes());
            assert_eq!(
                Record::decode(test_cell(3), &older),
                Err(Error::Format { cell: test_cell(3), format })
            );
        }
        assert!(matches!(Record::decode(test_cell(3), &bytes[..3]), Err(Error::Corrupt(_))));
    }

    /// Every kind of record, deleted or not, resumed or not, decodes back to itself.
    #[test]
    fn records_decode_to_what_they_encode() {
        let origin = Origin { parent: test_cell(1), at: 7 };
        let kinds =
            [Kind::Root, Kind::Own, Kind::Forking { origin, version: 0xbe }, Kind::Clone(origin)];
        for kind in kinds {
            for (deleted, resumed) in [(false, false), (true, false), (false, true), (true, true)] {
                let record = Record { deleted, resumed, ..kind.of(9) };
                assert_eq!(Record::decode(test_cell(2), &encode(&record)), Ok(record));
            }
        }
    }
}
