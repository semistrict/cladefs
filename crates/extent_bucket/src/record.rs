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
    /// ([`crate::branch::record_fork`]); or by its own host, once its fork of a point was made
    /// ([`crate::branch::fork_point`]).
    Fork(Origin),
    /// A point, from its first: no branch to write, but a name of its origin's state, which any
    /// number of hosts read and fork. It has no head, no segment and no checkpoint, and no writer
    /// takes it ([`Error::Point`]; doc-7, Points).
    Point(Origin),
    /// A fork of a point, or of a version, naming the origin it names as its own, until the fork is
    /// made ([`crate::branch::fork_point`], [`crate::branch::fork_version`]): until then it holds
    /// nothing, and nothing relies on it. The collector gives it up once that point is deleted, or
    /// that version is named by its branch's head no more ([`crate::branch::abandon`]).
    Forking { origin: Origin, of: Forked },
    /// cladefs: a fork of a point, made ([`crate::branch::finish_point_fork`]): what
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
            shared: false,
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
            | Self::Point(origin)
            | Self::Claim(origin) => Some(origin),
            Self::Forking { origin, .. } => Some(origin),
        }
    }
}

/// cladefs: what a fork being made is of ([`Kind::Forking`]): a point, by its branch; or a version
/// of the branch its origin names, at the segment its origin names, by the version's ID, which that
/// branch's head names while the version is kept (doc-18, TASK-476).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Forked {
    Point(Branch),
    Version(u64),
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
    /// cladefs: whether a point's forks may be of other tenants' cells than its own: a base image or
    /// a snapshot many tenants clone (TASK-395.7). Of a point alone, which its maker's trusted
    /// control API shares and unshares ([`crate::branch::share`]); a fork of another tenant's point
    /// not shared is refused as it reads the point's record ([`Record::may_fork_into`]). Unshared, a
    /// point's forks made stay.
    pub shared: bool,
    /// cladefs: of a fork of a point, being made or made, whether another than the host that began
    /// it resumed it ([`crate::branch::resume_point_fork`]): a store opening its child, its host
    /// dead or slow, or a fork of the point finding the record its own (TASK-455's review). Said
    /// with a condition, while the fork is being made, before the resume writes its reference:
    /// so a clone made says it if anyone but its host may write its reference after, whose
    /// retirement is then marked ([`Record::retires_unmarked`]).
    pub resumed: bool,
    /// cladefs: the branch's DEK, each generation of it, wrapped by its cell's KEK, oldest first:
    /// none before its first writer took it, nor of a point, which nothing writes (TASK-411.3);
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

    /// Whether it is a point ([`Kind::Point`]).
    pub fn is_point(&self) -> bool {
        matches!(self.kind, Kind::Point(_))
    }

    /// cladefs: whether a fork of `point`, whose record it is, may make a branch of a cell of
    /// `tenant`'s: of the point's own tenant, or of any if the point is shared (TASK-395.7).
    pub fn may_fork_into(&self, point: Branch, tenant: u64) -> bool {
        point.cell.tenant == tenant || self.shared
    }

    /// The point or version it is being forked from, until the fork is made ([`Kind::Forking`]).
    pub fn forking(&self) -> Option<Forked> {
        match self.kind {
            Kind::Forking { of, .. } => Some(of),
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

    /// Whether a writer may take the branch, and if not why: not once it is deleted; not a point,
    /// which nothing writes, whoever asks; nor a fork of a point not yet made, which holds nothing
    /// and may be given up on.
    pub fn writable(&self) -> Result<(), Error> {
        if self.deleted {
            return Err(Error::Deleted);
        }
        match self.kind {
            Kind::Point(_) if rules().points_take_no_writer => Err(Error::Point),
            Kind::Forking { .. } => Err(Error::NoBranch),
            _ => Ok(()),
        }
    }

    /// cladefs: whether the collector may remove it, with its cell, the cell dead, its branch's ID
    /// one an allocator gave, `given` ([`crate::objectid::given`]): deleted, a root's or a clone's
    /// made ([`Kind::Clone`]), records their makers alone make, once, no fork naming an
    /// allocator's ID (doc-7, Object IDs; TASK-455). Any other stays for good: a deleted fork's
    /// child's (and a point's, a claim's), a writer of its parent that is slow to record the fork,
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

    /// cladefs: it, of a fork of a point being made, said resumed ([`Record::resumed`]).
    pub fn resumed(self) -> Self {
        Self { resumed: true, ..self }
    }
}

/// A record as object storage holds it (doc-7, Layout): flags, which say its [`Kind`] together,
/// and whether a point is shared; cladefs: and its DEKs (TASK-411.3) and those it inherits
/// (TASK-411.4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Flags {
    format: u32,
    owner: u64,
    origin: Option<Origin>,
    /// Whether it is a fork's: of every kind but [`Kind::Root`].
    forked: bool,
    deleted: bool,
    point: bool,
    forking: Option<Branch>,
    /// cladefs: of a fork of a version being made, the version ([`Forked::Version`], TASK-476).
    forking_version: Option<u64>,
    claimed: bool,
    /// cladefs: a fork of a point, made ([`Kind::Clone`], TASK-455).
    cloned: bool,
    /// cladefs: of a point alone ([`Record::shared`], TASK-395.7).
    shared: bool,
    /// cladefs: of a fork of a point alone, being made or made ([`Record::resumed`]).
    resumed: bool,
    /// cladefs: [`Record::deks`].
    deks: Vec<Dek>,
    /// cladefs: [`Record::inherited`].
    inherited: Vec<InheritedDek>,
}

impl From<Record> for Flags {
    fn from(record: Record) -> Self {
        let Record { format, owner, kind, deleted, shared, resumed, deks, inherited } = record;
        let flags = Flags {
            format,
            owner,
            origin: kind.origin(),
            forked: kind != Kind::Root,
            deleted,
            point: false,
            forking: None,
            forking_version: None,
            claimed: false,
            cloned: false,
            shared,
            resumed,
            deks,
            inherited,
        };
        match kind {
            Kind::Root | Kind::Own | Kind::Fork(_) => flags,
            Kind::Clone(_) => Flags { cloned: true, ..flags },
            Kind::Point(_) => Flags { point: true, ..flags },
            Kind::Forking { of: Forked::Point(point), .. } => {
                Flags { forking: Some(point), ..flags }
            }
            Kind::Forking { of: Forked::Version(id), .. } => {
                Flags { forking_version: Some(id), ..flags }
            }
            Kind::Claim(_) => Flags { claimed: true, ..flags },
        }
    }
}

impl TryFrom<Flags> for Record {
    type Error = String;

    /// The record `flags` say, if they say a [`Kind`]: a fork's, of every kind but a root; an
    /// origin, of every kind but a root and one of its own; and one at most of being a point,
    /// being forked from one or from a version, being a claim and being a clone made, each of an
    /// origin. Shared only if a point; resumed only if being forked from one or a clone made.
    fn try_from(flags: Flags) -> Result<Self, String> {
        let forking = match (flags.forking, flags.forking_version) {
            (None, None) => None,
            (Some(point), None) => Some(Forked::Point(point)),
            (None, Some(id)) => Some(Forked::Version(id)),
            (Some(_), Some(_)) => return Err(format!("a branch's record of no kind: {flags:?}")),
        };
        let said = (flags.point, forking, flags.claimed, flags.cloned);
        let kind = match (flags.forked, flags.origin, said) {
            (false, None, (false, None, false, false)) => Kind::Root,
            (true, None, (false, None, false, false)) => Kind::Own,
            (true, Some(origin), (false, None, false, false)) => Kind::Fork(origin),
            (true, Some(origin), (true, None, false, false)) => Kind::Point(origin),
            (true, Some(origin), (false, Some(of), false, false)) => Kind::Forking { origin, of },
            (true, Some(origin), (false, None, true, false)) => Kind::Claim(origin),
            (true, Some(origin), (false, None, false, true)) => Kind::Clone(origin),
            _ => return Err(format!("a branch's record of no kind: {flags:?}")),
        };
        if flags.shared && !flags.point {
            return Err(format!("a branch's record shared, no point: {flags:?}"));
        }
        if flags.resumed && !matches!(kind, Kind::Forking { .. } | Kind::Clone(_)) {
            return Err(format!("a branch's record resumed, no fork of a point: {flags:?}"));
        }
        let Flags { format, owner, deleted, shared, resumed, deks, inherited, .. } = flags;
        Ok(Self { format, owner, kind, deleted, shared, resumed, deks, inherited })
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
        // 14's, whose keys' numbers are in hexadecimal (TASK-504).
        assert_eq!(FORMAT, 15);
        for format in [1u32, 2, 7, 8, 9, 10, 11, 12, 13, 14] {
            let mut older = bytes.clone();
            older[..4].copy_from_slice(&format.to_le_bytes());
            assert_eq!(
                Record::decode(test_branch(3), &older),
                Err(Error::Format { branch: test_branch(3), format })
            );
        }
        assert!(matches!(Record::decode(test_branch(3), &bytes[..3]), Err(Error::Corrupt(_))));
    }

    /// TASK-160.14: every combination of a record's ten flags (whether it names an origin, is a
    /// fork's, is deleted, is a point, is being forked from one, is a claim, is shared, is a clone
    /// made, TASK-455, was resumed, TASK-455's review, and is being forked from a version,
    /// TASK-476) decodes to the one kind it says, deleted or not, shared or not if a point, resumed
    /// or not if a fork of a point or a version, or is refused: 24 records, and 1,000 refused. What
    /// a record encodes decodes back to it.
    #[test]
    fn records_decode_to_the_kinds_their_flags_say() {
        let origin = Origin { parent: test_branch(1), at: 7 };
        let mut decoded = Vec::new();
        for bits in 0..=1023u16 {
            let flag = |bit: u16| bits & (1 << bit) != 0;
            let flags = Flags {
                format: FORMAT,
                owner: 9,
                origin: flag(0).then_some(origin),
                forked: flag(1),
                deleted: flag(2),
                point: flag(3),
                forking: flag(4).then_some(test_branch(5)),
                forking_version: flag(9).then_some(0xbe),
                claimed: flag(5),
                cloned: flag(7),
                shared: flag(6),
                resumed: flag(8),
                deks: Vec::new(),
                inherited: Vec::new(),
            };
            let said = Record::decode(test_branch(2), &encode(&flags)).ok();
            if let Some(record) = said {
                assert_eq!(Flags::from(record.clone()), flags, "{record:?}");
                assert_eq!(Record::decode(test_branch(2), &encode(&record)), Ok(record.clone()));
                decoded.push((bits, record.kind, record.deleted, record.shared, record.resumed));
            }
        }
        let forking = Kind::Forking { origin, of: Forked::Point(test_branch(5)) };
        let versioned = Kind::Forking { origin, of: Forked::Version(0xbe) };
        // Bits: origin 1, forked 2, deleted 4, point 8, forking 16, claimed 32, shared 64, cloned
        // 128, resumed 256, forking a version 512.
        let kinds = [
            (0, Kind::Root, false, false, false),
            (2, Kind::Own, false, false, false),
            (3, Kind::Fork(origin), false, false, false),
            (4, Kind::Root, true, false, false),
            (6, Kind::Own, true, false, false),
            (7, Kind::Fork(origin), true, false, false),
            (11, Kind::Point(origin), false, false, false),
            (15, Kind::Point(origin), true, false, false),
            (19, forking, false, false, false),
            (23, forking, true, false, false),
            (35, Kind::Claim(origin), false, false, false),
            (39, Kind::Claim(origin), true, false, false),
            (75, Kind::Point(origin), false, true, false),
            (79, Kind::Point(origin), true, true, false),
            (131, Kind::Clone(origin), false, false, false),
            (135, Kind::Clone(origin), true, false, false),
            (275, forking, false, false, true),
            (279, forking, true, false, true),
            (387, Kind::Clone(origin), false, false, true),
            (391, Kind::Clone(origin), true, false, true),
            (515, versioned, false, false, false),
            (519, versioned, true, false, false),
            (771, versioned, false, false, true),
            (775, versioned, true, false, true),
        ];
        assert_eq!(decoded, kinds);
        let refused = Flags {
            format: FORMAT,
            owner: 9,
            origin: None,
            forked: true,
            deleted: false,
            point: true,
            forking: None,
            forking_version: None,
            claimed: false,
            cloned: false,
            shared: false,
            resumed: false,
            deks: Vec::new(),
            inherited: Vec::new(),
        };
        let corrupt = format!(
            "a branch's record of no kind: Flags {{ format: {FORMAT}, owner: 9, origin: None, \
             forked: true, deleted: false, point: true, forking: None, forking_version: None, \
             claimed: false, cloned: false, shared: false, resumed: false, deks: [], inherited: [] }}"
        );
        let refused = Record::decode(test_branch(2), &encode(&refused));
        assert_eq!(refused, Err(Error::Corrupt(corrupt)));
        // Shared, of a fork: only a point is.
        let fork = Flags { shared: true, ..Flags::from(Kind::Fork(origin).of(9)) };
        let corrupt = format!("a branch's record shared, no point: {fork:?}");
        assert_eq!(Record::decode(test_branch(2), &encode(&fork)), Err(Error::Corrupt(corrupt)));
        // Resumed, of a writable fork: only a fork of a point is.
        let fork = Flags { resumed: true, ..Flags::from(Kind::Fork(origin).of(9)) };
        let corrupt = format!("a branch's record resumed, no fork of a point: {fork:?}");
        assert_eq!(Record::decode(test_branch(2), &encode(&fork)), Err(Error::Corrupt(corrupt)));
    }
}
