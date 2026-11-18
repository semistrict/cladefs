// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a cell's head (doc-7, The head; TASK-407, TASK-515): its one swapped object ([`Head`]),
//! what made it, its keys, its writer's taking and its commit. Every object of a cell's is written
//! once, at a key naming its writer's epoch, and is the cell's only once the head names it: a writer
//! acknowledges its segments by swapping the head to name them, with the condition that it is
//! still the version it last wrote, its own taking's. A takeover swaps it to the next epoch, so that
//! the writer before fails at its next swap: the head is the fence. A deletion swaps it to
//! [`Head::FINAL`], which no writer takes, and nothing swaps after.
//!
//! A taking is a host's placement of the cell and its writer's at once, one conditional write
//! ([`take`]): refused to a placement of a lower token than the one holding it ([`Error::Stale`]),
//! so that a placement cut off takes no cell back from the one after it (TASK-39.7). It gives the
//! cell what it lacks in the same write: a kind, a KEK, a DEK ([`Taking`]).
//!
//! Every swap is a conditional write of bytes unique to its writer, a head naming its holder, so
//! that one that landed though its answer was lost is recognised by reading it back.

use crate::bucket::{self, Bucket, Object, PutMode, Version};
use crate::cell::Kind;
use crate::keys::{Dek, InheritedDek, Kek};
use crate::layout::{self, CellId, Head};
use crate::{Error, rules};
use bytes::Bytes;

/// A head as its writer last swapped it, or a taking took it: what it holds, and the version its
/// next swap is conditional on.
#[derive(Clone, Debug)]
pub struct Held {
    pub head: Head,
    version: Version,
}

impl Held {
    /// The head as its write left it, raw: what a later change of it starts from
    /// ([`crate::lifecycle::Start::Found`]), a read less.
    pub fn object(&self, cell: CellId) -> Object {
        let bytes = layout::encode(&self.head);
        Object::written(&layout::head(cell), bytes, self.version.clone(), 0)
    }
}

/// `cell`'s head as a read finds it, raw, for a taking of it after ([`take`]).
pub struct Read(pub(crate) Option<Object>);

impl Read {
    /// What was found: the head as a change of it left it ([`Held::object`], TASK-449), which a
    /// taking goes on from, its write conditional on it as on what a read found.
    pub fn found(object: Option<Object>) -> Self {
        Self(object)
    }

    /// What was found, raw: what a host learns for an opening of the cell after
    /// ([`crate::known::Learned`]).
    pub fn object(self) -> Option<Object> {
        self.0
    }

    /// The head read, if there was one.
    pub fn head(&self, cell: CellId) -> Result<Option<Head>, Error> {
        self.0.as_ref().map(|object| Head::decode(cell, &object.bytes)).transpose()
    }
}

/// Reads `cell`'s head for a taking of it after ([`take`]), as with other reads a taking needs,
/// at once (TASK-165.11).
pub async fn read(bucket: &impl Bucket, cell: CellId) -> Read {
    Read(bucket::get(bucket, &layout::head(cell)).await)
}

/// `cell`'s head, if it has one: [`Error::Format`] if it is of another format than this one,
/// whose objects this reads none of.
pub async fn get(bucket: &impl Bucket, cell: CellId) -> Result<Option<Head>, Error> {
    read(bucket, cell).await.head(cell)
}

/// `cell`'s head, if it says the cell is live: an operator's look, to compare with what hosts
/// hold (`cladefs-gc --cells`, TASK-395.4). A fork whose child has no head yet isn't live.
pub async fn live(bucket: &impl Bucket, cell: CellId) -> Result<Option<Head>, Error> {
    Ok(get(bucket, cell).await?.filter(|head| !head.deleted()))
}

/// What a taking of a cell writes beside its epoch, holder and token: what the cell lacks, given
/// in the same write.
#[derive(Clone, Debug, Default)]
pub struct Taking {
    /// Who takes it: unique to each taking, so that a write of it is recognised.
    pub holder: u64,
    /// The fencing token of the taker's placement.
    pub token: u64,
    /// The kind the cell is of, given it if it has none: refused if it has another
    /// ([`Error::OtherKind`]), as a cell has one (doc-20).
    pub kind: Option<Kind>,
    /// cladefs: the keys given a head with none, as found when they were made
    /// ([`crate::keys::taking`]).
    pub keys: Option<Given>,
}

/// cladefs: keys a taking gives the head it takes, made for the head as found then
/// ([`crate::keys::taking`]): a KEK if it had none, and its first DEK, with those it inherits,
/// if it had none. Given only if the head is still without them: else the taking stops, its keys
/// to be made anew ([`Error::Raced`]).
#[derive(Clone, Debug)]
pub struct Given {
    pub kek: Option<Kek>,
    pub deks: Option<(Vec<Dek>, Vec<InheritedDek>)>,
}

impl Given {
    /// Whether these were made for `head`: its KEKs and DEKs as they found them.
    fn fits(&self, head: &Head) -> bool {
        self.kek.is_some() == head.keks.is_empty() && self.deks.is_some() == head.deks.is_empty()
    }
}

/// Takes `cell`'s head as [`Taking`] says, of the epoch after its last, starting from `read`,
/// what a read of it before found ([`read`]): its write is conditional on what `read` found, so
/// that another's taking since, which changed it, is read and gone on from. What it named, the
/// segments the cell holds and its newest checkpoint, kept. Refused for a cell with no head, which
/// its maker makes ([`crate::lifecycle::create`]) and a taking never ([`Error::NoCell`]); for a
/// deleted cell's ([`Error::Deleted`]), a shredded one's, a fork's not made ([`Error::NoCell`]),
/// one a later placement took, whose token is higher ([`Error::Stale`]), and one whose keys
/// changed from those [`Taking::keys`] were made for ([`Error::Raced`]).
pub async fn take(
    bucket: &impl Bucket,
    cell: CellId,
    read: Read,
    taking: &Taking,
) -> Result<Held, Error> {
    let Taking { holder, token, .. } = *taking;
    let key = layout::head(cell);
    let mut found = read.0;
    loop {
        let Some(object) = &found else { return Err(Error::NoCell) };
        let head = Head::decode(cell, &object.bytes)?;
        head.writable()?;
        if head.shredded {
            return Err(crate::keys::shredded(cell));
        }
        if rules().tokens && head.token > token {
            return Err(Error::Stale);
        }
        let kind = match (head.kind, taking.kind) {
            (Some(had), Some(kind)) if had != kind => return Err(Error::OtherKind(had.name())),
            (had, given) => had.or(given),
        };
        let mut next = Head { epoch: head.epoch + 1, holder, token, kind, ..head };
        // cladefs: naming what it named, its versions among them (rule `versions_carried`,
        // TASK-476); without the rule, as a test shows it needed, none.
        if !rules().versions_carried {
            next.versions.clear();
        }
        if let Some(given) = &taking.keys {
            if !given.fits(&next) {
                return Err(Error::Raced);
            }
            next.keks.extend(given.kek.clone());
            if let Some((deks, inherited)) = given.deks.clone() {
                (next.deks, next.inherited) = (deks, inherited);
            }
        }
        match write(bucket, &key, &next, Some(object.meta.version.clone())).await {
            Ok(held) => return Ok(held),
            Err(changed) => found = changed,
        }
    }
}

/// Swaps `cell`'s head from `held`, as its holder last wrote it, to `next`: with the condition
/// that it is still that version (rule `head_swap`), and so that taking still, which it fails
/// otherwise ([`Error::Fenced`]): another taking, or the cell's deletion or shredding, swapped it
/// since.
pub async fn swap(
    bucket: &impl Bucket,
    cell: CellId,
    held: &Held,
    next: Head,
) -> Result<Held, Error> {
    let key = layout::head(cell);
    if !rules().head_swap {
        // Written whatever is there: a test's, which shows the condition needed.
        let bytes = Bytes::from(layout::encode(&next));
        let version = overwrite(bucket, &key, bytes).await;
        return Ok(Held { head: next, version });
    }
    write(bucket, &key, &next, Some(held.version.clone())).await.map_err(|_| Error::Fenced)
}

/// Writes `head` at `key`: created where `version` is none, else updated from it. A write that
/// landed though its answer was lost, its bytes read back, is its own. Fails with what is there
/// now, if another's write came first.
async fn write(
    bucket: &impl Bucket,
    key: &str,
    head: &Head,
    version: Option<Version>,
) -> Result<Held, Option<Object>> {
    let bytes = Bytes::from(layout::encode(head));
    let mode = match &version {
        None => PutMode::Create,
        Some(version) => PutMode::Update(version.clone()),
    };
    let mut failures = 0;
    loop {
        match bucket.put(key, bytes.clone(), mode.clone()).await {
            Ok(version) => return Ok(Held { head: head.clone(), version }),
            Err(
                bucket::Error::AlreadyExists
                | bucket::Error::Precondition
                | bucket::Error::NotFound,
            ) => {
                return match bucket::get(bucket, key).await {
                    Some(object) if object.bytes == bytes => {
                        Ok(Held { head: head.clone(), version: object.meta.version })
                    }
                    found => Err(found),
                };
            }
            Err(bucket::Error::Unavailable) => {
                failures += 1;
                bucket.pause(failures).await;
            }
        }
    }
}

/// Writes `bytes` at `key` whatever is there: its version.
async fn overwrite(bucket: &impl Bucket, key: &str, bytes: Bytes) -> Version {
    let mut failures = 0;
    loop {
        match bucket.put(key, bytes.clone(), PutMode::Overwrite).await {
            Ok(version) => return version,
            Err(_) => {
                failures += 1;
                bucket.pause(failures).await;
            }
        }
    }
}
