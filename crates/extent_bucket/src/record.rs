// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a branch's record: what made it, which is what it is ([`Kind`]), whose it is, whether
//! it was deleted (doc-7, Layout; TASK-160.14), and, of a point, whether other tenants may fork it
//! ([`Record::shared`], TASK-395.7); cladefs: and its DEKs, wrapped by its cell's KEK, if its cell
//! is encrypted ([`Record::deks`], doc-16, TASK-411.3).
//!
//! In object storage a record is flags, as doc-7 lists them; read, it is decoded once, into the
//! one kind its flags say, and refused if they say none ([`Error::Corrupt`]). What a record allows
//! is asked of it here, not of its flags where it is read; and every change of one goes through
//! [`crate::branch`]'s one transition.

use crate::keys::{Dek, InheritedDek};
use crate::layout::{Branch, FORMAT, Origin, decode};
use crate::{Error, rules};
use serde::{Deserialize, Serialize};

/// What made a branch, and so what it is: the kinds of record there are, each of the flags that
/// say it in object storage and of no others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A branch of no parent, which its maker created ([`crate::branch::create`]).
    Root,
    /// A fork's child of no parent, whose fork was made nowhere in object storage: a branch of its
    /// own ([`crate::branch::create_forked`]); or one a deletion found no record of, a fork's
    /// child before its record or no branch ([`crate::branch::delete`]).
    Own,
    /// A fork's child, forked from its origin's state: given its record by its parent's writers,
    /// or its parent's deletion, once the fork's segment was acknowledged
    /// ([`crate::branch::record_fork`]).
    Fork(Origin),
    /// A fork of a version, naming the origin the version names as its own, until the fork is made
    /// ([`crate::branch::fork_version`]): until then it holds nothing, and nothing relies on it.
    /// The collector gives it up once its branch's head names the version no more
    /// ([`crate::branch::abandon`]).
    Forking { origin: Origin, version: u64 },
    /// cladefs: a fork of a version, made ([`crate::branch::finish_version_fork`]): what
    /// [`Kind::Forking`] becomes, a fork's child as [`Kind::Fork`] is, but given its record by its
    /// own host alone, which no parent's writer records, no state naming the fork; and its
    /// reference, written before it was made, by that host alone (TASK-455). So, of an ID an
    /// allocator gave it, deleted, it goes with its cell ([`Record::removable`]), and its reference
    /// retires unmarked ([`Record::retires_unmarked`]).
    Clone(Origin),
    /// A failed fork's deletion's claim of its child, from what its host knew of the fork, not its
    /// parent's writers from an acknowledged segment ([`crate::branch::delete_fork`]): what it
    /// names may have gone before it landed, no record or state keeping that, so that recovering
    /// it failing says nothing was lost, if no writer has it ([`crate::branch::lost_is_ok`]).
    Claim(Origin),
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

    /// The branch it was forked from, and the segment of that branch's at whose end, if any.
    pub fn origin(self) -> Option<Origin> {
        match self {
            Self::Root | Self::Own => None,
            Self::Fork(origin)
            | Self::Clone(origin)
            | Self::Claim(origin) => Some(origin),
            Self::Forking { origin, .. } => Some(origin),
        }
    }
}

/// A branch's record: of its kind, which says where it came from, and whether it was deleted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "Flags", try_from = "Flags")]
pub struct Record {
    /// The format of the branch's objects ([`FORMAT`]), first, so that a reader of any format
    /// reads it before the rest, which it can read only if it is of its own ([`Record::decode`],
    /// TASK-141.2, TASK-165.3).
    pub format: u32,
    /// The branch's owner: a number its maker chose at random, unique to that making, which every
    /// record of the branch names, whoever writes it: a fork's host, its parent's writers and its
    /// parent's deletion alike (the fork's footer names it, [`crate::layout::Child::owner`]). So
    /// no two makers of one branch ID write the same bytes, and a create that finds a record there
    /// tells its own, landed though its answer was lost, from another's (TASK-79.82, TASK-153.9).
    pub owner: u64,
    pub kind: Kind,
    /// A deleted branch's record stays, which a writer of a parent that is slow to record a fork to
    /// its ID could make again were it removed; but a given root's and clone's, which go with
    /// their cell once it is dead ([`Record::removable`]).
    pub deleted: bool,
    /// cladefs: of a fork of a version, being made or made, whether another than the host that
    /// began it resumed it ([`crate::branch::resume_version_fork`]): a store opening its child, its
    /// host dead or slow, or a fork of the version finding the record its own (TASK-455's
    /// review). Said
    /// with a condition, while the fork is being made, before the resume writes its reference:
    /// so a clone made says it if anyone but its host may write its reference after, whose
    /// retirement is then marked ([`Record::retires_unmarked`]).
    pub resumed: bool,
    /// cladefs: the branch's DEK, each generation of it, wrapped by its cell's KEK, oldest first:
    /// none before its first writer took it (TASK-411.3);
    /// a writer's ring of a record with none is refused (TASK-411.28). Given by its writer ([`crate::keys::with_dek`]), whoever made
    /// the record: the parent's writers recording a fork write none, so that they write the same.
    pub deks: Vec<Dek>,
    /// cladefs: the DEKs of every ancestor of another cell whose objects the branch reads, each
    /// generation, rewrapped under its own cell's KEK (doc-16, Forks and shared points;
    /// TASK-411.4): given with its first DEK, in one conditional update, so that a record with
    /// DEKs holds all it inherits from other cells, its readers needing no other cell's KEK, nor
    /// another tenant's KMS. By its fork's host, as it makes the fork
    /// ([`crate::keys::give_inherited`]), or its first writer ([`crate::keys::with_dek`]).
    pub inherited: Vec<InheritedDek>,
}

impl Record {
    /// The record of `branch` that `bytes` encode, unless it is of another format than this one,
    /// newer or older, which its first bytes say ([`Error::Format`]), or its flags say no [`Kind`]
    /// ([`Error::Corrupt`]).
    pub fn decode(branch: Branch, bytes: &[u8]) -> Result<Self, Error> {
        let format: u32 = decode(bytes.get(..4).unwrap_or(bytes))?;
        if format != FORMAT {
            return Err(Error::Format { branch, format });
        }
        decode(bytes)
    }

    /// The branch it was forked from, and the segment of that branch's at whose end, if any.
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

    /// Whether it is the record of the child of the fork whose child's owner is `owner`, whoever
    /// wrote it: the fork's host, as it claimed the child or took a branch of its own for it
    /// ([`crate::branch::create_forked`]), or its parent's writers. Any other is another branch's,
    /// whose ID the fork's guest named (TASK-79.82).
    pub fn is_child_of(&self, owner: u64) -> bool {
        self.kind != Kind::Root && self.owner == owner
    }

    /// Whether a writer may take the branch, and if not why: not once it is deleted; nor a fork of
    /// a version not yet made, which holds nothing and may be given up on.
    pub fn writable(&self) -> Result<(), Error> {
        if self.deleted {
            return Err(Error::Deleted);
        }
        match self.kind {
            Kind::Forking { .. } => Err(Error::NoBranch),
            _ => Ok(()),
        }
    }

    /// cladefs: whether the collector may remove it, with its cell, the cell dead, its branch's ID
    /// one an allocator gave, `given` ([`crate::objectid::given`]): deleted, a root's or a clone's
    /// made ([`Kind::Clone`]), records their makers alone make, once, no fork naming an
    /// allocator's ID (doc-7, Object IDs; TASK-455). Any other stays for good: a deleted fork's
    /// child's (and a claim's), a writer of its parent that is slow to record the fork,
    /// or its next writer, could make again, the branch live again naming what the collector
    /// reclaimed; a deletion's of a branch it found none of ([`Kind::Own`]), which may be a fork's
    /// child yet to be recorded; a clone's never made, whose host may yet write its reference; and
    /// any of an ID no allocator gave, a guest's, which a fork may name too, its segment
    /// acknowledged and its child not yet recorded as a root took the ID (TASK-174). Without the
    /// rule that they stay, any deleted record goes; without the rule that IDs are given, any of
    /// a root's or a clone's, whatever its ID.
    pub fn removable(&self, given: bool) -> bool {
        let rules = rules();
        if !rules.records_kept {
            return self.deleted;
        }
        self.deleted
            && matches!(self.kind, Kind::Root | Kind::Clone(_))
            && (given || !rules.ids_given)
    }

    /// cladefs: whether the reference of its fork, its branch deleted, is retired unmarked, its
    /// branch's ID one an allocator gave, `given`: a clone's made by its host alone, its one
    /// writer, which wrote it before it made the clone, and which no recorder writes, no fork
    /// naming the ID (TASK-455). A mark is for a writer of it whose read of the child's record came
    /// before the retirement (TASK-395.13, TASK-395.14): none is left. A clone resumed by another
    /// ([`Record::resumed`]) is marked: its host, slow, may write it after (rule `resumes_marked`).
    /// Without the rule `clones_made_unmarked`, any clone's, being made or made, and a writable
    /// fork's still not.
    pub fn retires_unmarked(&self, given: bool) -> bool {
        let rules = rules();
        match rules.clones_made_unmarked {
            true => {
                matches!(self.kind, Kind::Clone(_))
                    && given
                    && !(self.resumed && rules.resumes_marked)
            }
            false => matches!(self.kind, Kind::Clone(_) | Kind::Forking { .. }),
        }
    }

    /// cladefs: it, of a fork of a version being made, said resumed ([`Record::resumed`]).
    pub fn resumed(self) -> Self {
        Self { resumed: true, ..self }
    }
}

/// A record as object storage holds it (doc-7, Layout): flags, which say its [`Kind`] together;
/// cladefs: and its DEKs (TASK-411.3) and those it inherits (TASK-411.4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Flags {
    format: u32,
    owner: u64,
    origin: Option<Origin>,
    /// Whether it is a fork's: of every kind but [`Kind::Root`].
    forked: bool,
    deleted: bool,
    /// cladefs: of a fork of a version being made, the version ([`Kind::Forking`], TASK-476).
    forking: Option<u64>,
    claimed: bool,
    /// cladefs: a fork of a version, made ([`Kind::Clone`], TASK-455).
    cloned: bool,
    /// cladefs: of a fork of a version alone, being made or made ([`Record::resumed`]).
    resumed: bool,
    /// cladefs: [`Record::deks`].
    deks: Vec<Dek>,
    /// cladefs: [`Record::inherited`].
    inherited: Vec<InheritedDek>,
}

impl From<Record> for Flags {
    fn from(record: Record) -> Self {
        let Record { format, owner, kind, deleted, resumed, deks, inherited } = record;
        let flags = Flags {
            format,
            owner,
            origin: kind.origin(),
            forked: kind != Kind::Root,
            deleted,
            forking: None,
            claimed: false,
            cloned: false,
            resumed,
            deks,
            inherited,
        };
        match kind {
            Kind::Root | Kind::Own | Kind::Fork(_) => flags,
            Kind::Clone(_) => Flags { cloned: true, ..flags },
            Kind::Forking { version, .. } => Flags { forking: Some(version), ..flags },
            Kind::Claim(_) => Flags { claimed: true, ..flags },
        }
    }
}

impl TryFrom<Flags> for Record {
    type Error = String;

    /// The record `flags` say, if they say a [`Kind`]: a fork's, of every kind but a root; an
    /// origin, of every kind but a root and one of its own; and one at most of being forked from
    /// a version, being a claim and being a clone made, each of an origin. Resumed only if being
    /// forked or a clone made.
    fn try_from(flags: Flags) -> Result<Self, String> {
        let said = (flags.forking, flags.claimed, flags.cloned);
        let kind = match (flags.forked, flags.origin, said) {
            (false, None, (None, false, false)) => Kind::Root,
            (true, None, (None, false, false)) => Kind::Own,
            (true, Some(origin), (None, false, false)) => Kind::Fork(origin),
            (true, Some(origin), (Some(version), false, false)) => {
                Kind::Forking { origin, version }
            }
            (true, Some(origin), (None, true, false)) => Kind::Claim(origin),
            (true, Some(origin), (None, false, true)) => Kind::Clone(origin),
            _ => return Err(format!("a branch's record of no kind: {flags:?}")),
        };
        if flags.resumed && !matches!(kind, Kind::Forking { .. } | Kind::Clone(_)) {
            return Err(format!("a branch's record resumed, no fork of a version: {flags:?}"));
        }
        let Flags { format, owner, deleted, resumed, deks, inherited, .. } = flags;
        Ok(Self { format, owner, kind, deleted, resumed, deks, inherited })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::encode;
    use crate::layout::test_branch;

    /// A record says its format first: one of another format, newer or older, is refused, naming
    /// it, whatever follows, which this format doesn't read (TASK-141.2, TASK-165.3).
    #[test]
    fn records_of_other_formats_are_refused() {
        let record = Kind::Own.of(7);
        let bytes = encode(&record);
        assert_eq!(&bytes[..4], &FORMAT.to_le_bytes());
        assert_eq!(Record::decode(test_branch(3), &bytes), Ok(record));
        let mut newer = (FORMAT + 1).to_le_bytes().to_vec();
        newer.extend_from_slice(b"what a later format holds");
        assert_eq!(
            Record::decode(test_branch(3), &newer),
            Err(Error::Format { branch: test_branch(3), format: FORMAT + 1 })
        );
        // Format 1's and 2's records, whose footers and checkpoints this format reads wrong; 7's,
        // whose points say nothing of being shared (TASK-395.7); 8's, whose collector's lease
        // and moves are at the bucket's root, its cells' collection under their own prefixes
        // reading neither (TASK-395.2); and 9's, whose records and cells keep no keys, their
        // objects framed in FORMAT 1 of extent_frame (TASK-411.3); and
        // 10's, whose keys name no epochs, and whose branches have leases, not heads (TASK-407);
        // and 11's, whose clones made say they are forks a recorder may record (TASK-455); and
        // 12's, whose forks of points say nothing of being resumed (TASK-455's review); and 13's,
        // whose heads name no versions, nor footers the versions cut at their ends (TASK-476); and
        // 14's, whose keys' numbers are in hexadecimal (TASK-504); 15's, whose cells name maps
        // of vdisks (TASK-508.1); and 16's, whose records name points (TASK-508.2).
        assert_eq!(FORMAT, 17);
        for format in [1u32, 2, 7, 8, 9, 10, 11, 12, 13, 14, 15] {
            let mut older = bytes.clone();
            older[..4].copy_from_slice(&format.to_le_bytes());
            assert_eq!(
                Record::decode(test_branch(3), &older),
                Err(Error::Format { branch: test_branch(3), format })
            );
        }
        assert!(matches!(Record::decode(test_branch(3), &bytes[..3]), Err(Error::Corrupt(_))));
    }

    /// TASK-160.14: every combination of a record's seven flags (whether it names an origin, is a
    /// fork's, is deleted, is being forked from a version, TASK-476, is a claim, is a clone made,
    /// TASK-455, and was resumed, TASK-455's review) decodes to the one kind it says, deleted or not,
    /// resumed or not if a fork of a version, or is refused: 16 records, and 112 refused. What a
    /// record encodes decodes back to it.
    #[test]
    fn records_decode_to_the_kinds_their_flags_say() {
        let origin = Origin { parent: test_branch(1), at: 7 };
        let mut decoded = Vec::new();
        for bits in 0..=127u16 {
            let flag = |bit: u16| bits & (1 << bit) != 0;
            let flags = Flags {
                format: FORMAT,
                owner: 9,
                origin: flag(0).then_some(origin),
                forked: flag(1),
                deleted: flag(2),
                forking: flag(3).then_some(0xbe),
                claimed: flag(4),
                cloned: flag(5),
                resumed: flag(6),
                deks: Vec::new(),
                inherited: Vec::new(),
            };
            let said = Record::decode(test_branch(2), &encode(&flags)).ok();
            if let Some(record) = said {
                assert_eq!(Flags::from(record.clone()), flags, "{record:?}");
                assert_eq!(Record::decode(test_branch(2), &encode(&record)), Ok(record.clone()));
                decoded.push((bits, record.kind, record.deleted, record.resumed));
            }
        }
        let forking = Kind::Forking { origin, version: 0xbe };
        // Bits: origin 1, forked 2, deleted 4, forking 8, claimed 16, cloned 32, resumed 64.
        let kinds = [
            (0, Kind::Root, false, false),
            (2, Kind::Own, false, false),
            (3, Kind::Fork(origin), false, false),
            (4, Kind::Root, true, false),
            (6, Kind::Own, true, false),
            (7, Kind::Fork(origin), true, false),
            (11, forking, false, false),
            (15, forking, true, false),
            (19, Kind::Claim(origin), false, false),
            (23, Kind::Claim(origin), true, false),
            (35, Kind::Clone(origin), false, false),
            (39, Kind::Clone(origin), true, false),
            (75, forking, false, true),
            (79, forking, true, true),
            (99, Kind::Clone(origin), false, true),
            (103, Kind::Clone(origin), true, true),
        ];
        assert_eq!(decoded, kinds);
        let refused = Flags {
            format: FORMAT,
            owner: 9,
            origin: None,
            forked: false,
            deleted: false,
            forking: None,
            claimed: true,
            cloned: false,
            resumed: false,
            deks: Vec::new(),
            inherited: Vec::new(),
        };
        let corrupt = format!(
            "a branch's record of no kind: Flags {{ format: {FORMAT}, owner: 9, origin: None, \
             forked: false, deleted: false, forking: None, claimed: true, cloned: false, \
             resumed: false, deks: [], inherited: [] }}"
        );
        let refused = Record::decode(test_branch(2), &encode(&refused));
        assert_eq!(refused, Err(Error::Corrupt(corrupt)));
        // Resumed, of a writable fork: only a fork of a version is.
        let fork = Flags { resumed: true, ..Flags::from(Kind::Fork(origin).of(9)) };
        let corrupt = format!("a branch's record resumed, no fork of a version: {fork:?}");
        assert_eq!(Record::decode(test_branch(2), &encode(&fork)), Err(Error::Corrupt(corrupt)));
    }
}
