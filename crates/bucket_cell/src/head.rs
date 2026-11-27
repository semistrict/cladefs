// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a cell's head (doc-7, The head; TASK-407, TASK-515): its one swapped object ([`Head`]),
//! what made it, its keys, its writer's taking and its commit. Every object of a cell's is written
//! once, at a key naming its writer's epoch, and is the cell's only once the head names it: a
//! writer acknowledges its segments by swapping the head to name them, with the condition that it
//! is still the version it last wrote, its own taking's. A takeover swaps it to the next epoch, so
//! that the writer before fails at its next swap: the head is the fence. A deletion swaps it to
//! [`Head::FINAL`], which no writer takes, and nothing swaps after.
//!
//! A taking is a host's placement of the cell and its writer's at once, one conditional write
//! ([`take`]): refused to a placement of a lower token than the one holding it ([`Error::Stale`]),
//! so that a placement cut off takes no cell back from the one after it (TASK-39.7). It gives the
//! cell what it lacks in the same write: a kind, a KEK, a DEK ([`Taking`]).
//!
//! Every swap is a conditional write of bytes unique to its writer, a head naming its holder, so
//! that one that landed though its answer was lost is recognised by reading it back.
//!
//! cladefs: the head is one object (doc-27 step 4): every swap writes it in place, conditional on
//! the version of it its writer last saw, and a making creates it, one create, so that it is never
//! half made. A writer that held a cell removed since fails its next swap: the condition is on an
//! object no longer there, which nothing but its maker's create makes. GCS takes about one write a
//! second of an object: its swaps are spaced so ([`crate::bucket::Bucket::rewritable`], TASK-690),
//! each naming all that landed since.

use crate::bucket::{self, Bucket, Created, PutMode, Version};
use crate::cell::Kind;
use crate::keys::{Dek, InheritedDek, Kek};
use crate::layout::{self, CellId, Head};
use crate::lifecycle::{Start, Step};
use crate::{Error, rules};
use bytes::Bytes;

/// A head as its writer last swapped it, or a taking took it: what it holds, and the version of
/// its object, which its next swap is conditional on.
#[derive(Clone, Debug)]
pub struct Held {
    pub head: Head,
    version: Version,
}

impl Held {
    /// The head as its write left it: what a later change of it starts from
    /// ([`crate::lifecycle::Start::Found`]), a read less.
    pub fn read(&self) -> Read {
        Read { version: Some(self.version.clone()), head: Ok(Some(self.head.clone())) }
    }
}

/// `cell`'s head as a read, or a write, found it: its object's version, if there is one, which a
/// write of it after is conditional on, and what it holds.
#[derive(Clone, Debug)]
pub struct Read {
    version: Option<Version>,
    /// The head, if there is one; [`Error::Format`] if its object is of another format than this
    /// one, whose objects this reads none of.
    head: Result<Option<Head>, Error>,
}

impl Read {
    /// No head: where a maker makes one.
    pub(crate) fn none() -> Self {
        Self { version: None, head: Ok(None) }
    }

    /// What `object`, the head's if found, says of `cell`'s head.
    fn of(cell: CellId, object: Option<bucket::Object>) -> Self {
        match object {
            None => Self::none(),
            Some(object) => Self {
                version: Some(object.meta.version.clone()),
                head: Head::decode(cell, &object.bytes).map(Some),
            },
        }
    }

    /// The head read, if there was one: [`Error::Format`] if it is of another format than this
    /// one.
    pub fn head(&self) -> Result<Option<Head>, Error> {
        self.head.clone()
    }
}

/// Reads `cell`'s head, one GET, for a taking of it after ([`take`]), as with other reads a taking
/// needs, at once (TASK-165.11). cladefs: refused, [`bucket::Refused`] (board 5861).
pub async fn read(bucket: &impl Bucket, cell: CellId) -> Result<Read, bucket::Refused> {
    Ok(Read::of(cell, bucket::get(bucket, &layout::head(cell)).await?))
}

/// `cell`'s head, if it has one: [`Error::Format`] if it is of another format than this one,
/// whose objects this reads none of.
pub async fn get(bucket: &impl Bucket, cell: CellId) -> Result<Option<Head>, Error> {
    read(bucket, cell).await?.head()
}

/// `cell`'s head, if it says the cell is live: an operator's look, to compare with what hosts
/// hold (`cladefs-gc --cells`, TASK-395.4). A fork whose child has no head yet isn't live.
pub async fn live(bucket: &impl Bucket, cell: CellId) -> Result<Option<Head>, Error> {
    Ok(get(bucket, cell).await?.filter(|head| !head.deleted()))
}

/// What a taking of a cell writes beside its epoch, holder and token: what the cell lacks, given
/// in the same write.
#[derive(Clone, Default)]
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
    /// cladefs: what the cell's fast WAL commit mode is to be (doc-29): `None` keeps what the head
    /// says, `Some(Some(fsync))` enables fast WAL with `fsync` (`Ordered` or `FastWal`) while
    /// preserving any `active` WAL epoch/bucket so the taker fences and flushes it first
    /// (`keepOldWalInHead`), and `Some(None)` disables fast WAL (preserving `active` if set until
    /// recovery flushes and clears it).
    pub fast_wal: Option<Option<layout::Fsync>>,
    /// cladefs: allowlist check applied to any persisted `head.fast_wal.active.bucket` URL on
    /// every CAS attempt (including `Unwritten::Changed` retries) before writing the taken head
    /// (doc-29, Hotbed review 118).
    pub fast_wal_allowed: Option<std::sync::Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>>,
}

impl std::fmt::Debug for Taking {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Taking")
            .field("holder", &self.holder)
            .field("token", &self.token)
            .field("kind", &self.kind)
            .field("keys", &self.keys)
            .field("fast_wal", &self.fast_wal)
            .field("fast_wal_allowed", &self.fast_wal_allowed.is_some())
            .finish()
    }
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
/// its maker makes ([`crate::lifecycle::create`]) and a taking never ([`Error::NoCell`]): one being
/// made, or removed, too; for a deleted cell's ([`Error::Deleted`]), a shredded one's, a fork's not
/// made ([`Error::NoCell`]), one a later placement took, whose token is higher ([`Error::Stale`]),
/// and one whose keys changed from those [`Taking::keys`] were made for ([`Error::Raced`]).
pub async fn take(
    bucket: &impl Bucket,
    cell: CellId,
    found: Read,
    taking: &Taking,
) -> Result<Held, Error> {
    let Taking { holder, token, .. } = *taking;
    let mut found = found;
    loop {
        let Some(head) = found.head()? else { return Err(Error::NoCell) };
        head.writable()?;
        if head.shredded {
            return Err(crate::keys::shredded(cell));
        }
        if rules().tokens && head.token > token {
            return Err(Error::Stale);
        }
        if head.replication.is_some() {
            return Err(Error::Corrupt(format!(
                "{cell}: cell head names peer replication (doc-21), which has been removed; \
                 refusing to open as a Standard cell because acknowledged commits may sit on \
                 peers alone"
            )));
        }
        let kind = match (head.kind, taking.kind) {
            (Some(had), Some(kind)) if had != kind => return Err(Error::OtherKind(had.name())),
            (had, given) => had.or(given),
        };
        let mut next = Head { epoch: head.epoch + 1, holder, token, kind, ..head.clone() };
        // cladefs: naming what it named, its versions among them (rule `versions_carried`,
        // TASK-476); without the rule, as a test shows it needed, none.
        if !rules().versions_carried {
            next.versions.clear();
        }
        if let Some(fast_wal) = taking.fast_wal {
            // cladefs: kept until the taker's activation replaces it (`wal_kept_on_take`).
            let active = next.fast_wal.take().and_then(|had| had.active);
            let active = active.filter(|_| rules().wal_kept_on_take);
            next.fast_wal = match fast_wal {
                Some(fsync) => Some(layout::FastWal { fsync, active }),
                // Preserve any active fast WAL until recovery flushes it and clears fast_wal.
                None => active.map(|active| layout::FastWal {
                    fsync: layout::Fsync::Ordered,
                    active: Some(active),
                }),
            };
        }
        if next.fast_wal.is_some()
            && let Some(check) = &taking.fast_wal_allowed
            && let Some(active) = next.fast_wal.as_ref().and_then(|fw| fw.active.as_ref())
        {
            check(&active.bucket).map_err(|msg| Error::Corrupt(format!("{cell}: {msg}")))?;
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
        match write(bucket, cell, &found, next).await {
            Ok(held) => return Ok(held),
            Err(Unwritten::Changed) => found = read(bucket, cell).await?,
            Err(Unwritten::Refused(refused)) => return Err(refused.into()),
        }
    }
}

/// Swaps `cell`'s head from `held`, as its holder last wrote it, to `next`: with the condition
/// that it is still what that write left (rule `head_swap`), and so that taking still, which it
/// fails otherwise ([`Error::Fenced`]): another taking, or the cell's deletion or shredding,
/// swapped it since, or the collector removed it.
pub async fn swap(
    bucket: &impl Bucket,
    cell: CellId,
    held: &Held,
    next: Head,
) -> Result<Held, Error> {
    write(bucket, cell, &held.read(), next).await.map_err(|unwritten| match unwritten {
        // cladefs: refused by the bucket, not fenced (board 5861).
        Unwritten::Refused(refused) => refused.into(),
        Unwritten::Changed => Error::Fenced,
    })
}

/// Why a [`write`] wrote nothing.
#[derive(Debug)]
pub(crate) enum Unwritten {
    /// Another's write came first, or the head is no longer there, removed: no write but its
    /// maker's creates it.
    Changed,
    /// cladefs: the bucket refused the write, for good (board 5861).
    Refused(bucket::Refused),
}

impl From<bucket::Refused> for Unwritten {
    fn from(refused: bucket::Refused) -> Self {
        Self::Refused(refused)
    }
}

/// Writes `next`, the head after the one `found` holds, conditional on it being still what
/// `found` saw (rule `head_swap`; without it, as a test shows it needed, whatever is there). A
/// write that landed though its answer was lost, its bytes read back, is its own.
async fn write(
    bucket: &impl Bucket,
    cell: CellId,
    found: &Read,
    next: Head,
) -> Result<Held, Unwritten> {
    // One more than the head found's: bytes of its own ([`Head::swaps`]).
    let swaps = found.head.as_ref().ok().and_then(Option::as_ref).map_or(0, |head| head.swaps + 1);
    let next = Head { format: layout::FORMAT.current, swaps, ..next };
    let key = layout::head(cell);
    let bytes = layout::encode(&next);
    let version = match rules().head_swap {
        true => {
            // A head found was found in an object: a write of none is a maker's ([`birth`]).
            let Some(seen) = &found.version else { return Err(Unwritten::Changed) };
            match bucket::update(bucket, &key, bytes, seen).await? {
                bucket::Updated::Updated(version) => version,
                bucket::Updated::Changed => return Err(Unwritten::Changed),
            }
        }
        false => overwrite(bucket, &key, Bytes::from(bytes)).await?,
    };
    Ok(Held { head: next, version })
}

/// Makes `cell`'s head `head`, its maker's: one create, which decides whose the cell is, its swaps
/// none yet. One there already, another's, is what it fails with ([`Unwritten::Changed`]);
/// refused, [`Unwritten::Refused`].
pub(crate) async fn birth(
    bucket: &impl Bucket,
    cell: CellId,
    head: Head,
) -> Result<Held, Unwritten> {
    let head = Head { format: layout::FORMAT.current, swaps: 0, ..head };
    match bucket::create(bucket, &layout::head(cell), layout::encode(&head)).await? {
        Created::Created(version) => Ok(Held { head, version }),
        Created::Exists(_) => Err(Unwritten::Changed),
    }
}

/// Changes `cell`'s head as `step` says of what is there, starting from `start`, until it writes
/// nothing or what it writes lands, read again whenever another's write came first: a head made
/// ([`birth`]) where none is, the next swap of one there ([`write`]). With the head as it left it.
pub(crate) async fn change_leaving<R>(
    bucket: &impl Bucket,
    cell: CellId,
    start: Start<Read>,
    mut step: impl FnMut(Option<Head>) -> Result<Step<Head, R>, Error>,
) -> Result<(R, Read), Error> {
    let mut found = match start {
        Start::Read => read(bucket, cell).await?,
        Start::Create => Read::none(),
        Start::Found(found) => found,
    };
    loop {
        let current = found.head()?;
        let (next, result) = match step(current.clone())? {
            Step::Stop(result) => return Ok((result, found)),
            Step::Write(next, result) => (next, result),
        };
        let written = match &current {
            None => birth(bucket, cell, next).await,
            Some(_) => write(bucket, cell, &found, next).await,
        };
        match written {
            Ok(held) => return Ok((result, held.read())),
            Err(Unwritten::Changed) => found = read(bucket, cell).await?,
            Err(Unwritten::Refused(refused)) => return Err(refused.into()),
        }
    }
}

/// Writes `bytes` at `key` whatever is there: its version; refused, [`bucket::Refused`].
async fn overwrite(
    bucket: &impl Bucket,
    key: &str,
    bytes: Bytes,
) -> Result<Version, bucket::Refused> {
    let mut failures = 0;
    loop {
        match bucket.put(key, bytes.clone(), PutMode::Overwrite).await {
            Ok(version) => return Ok(version),
            Err(bucket::Error::Refused(refusal)) => {
                return Err(bucket::Refused { key: key.to_owned(), refusal });
            }
            // Out of reach: asked again, as every request of a bucket is.
            Err(_) => {
                failures += 1;
                bucket.pause(failures).await;
            }
        }
    }
}

#[cfg(all(test, feature = "full"))]
mod tests {
    use super::*;
    use crate::layout::test_cell;
    use crate::testing::SimBucket;

    const CELL: CellId = test_cell(3);

    fn taking(holder: u64) -> Taking {
        Taking { holder, ..Taking::default() }
    }

    /// doc-27 step 4 (board 6220's AC #1): a cell's making is one create of its head, a read of it
    /// one GET, a taking one conditional write of it, in place, and each swap after one more; a
    /// writer behind fails its swap.
    #[test]
    fn heads_are_one_object_made_read_and_swapped_in_place() {
        let bucket = SimBucket::new();
        let key = layout::head(CELL);
        let (made, asked) = bucket.served(crate::lifecycle::create(&*bucket, CELL, 7));
        assert_eq!((made, asked), (Ok(true), vec![format!("put {key}")]));
        let (found, asked) = bucket.served(read(&*bucket, CELL));
        assert_eq!(asked, [format!("get {key}")]);
        let found = found.unwrap();
        assert_eq!(found.head().unwrap().map(|head| (head.owner, head.epoch)), Some((7, 0)));

        let (held, asked) = bucket.served(take(&*bucket, CELL, found, &taking(1)));
        let mut held = held.unwrap();
        assert_eq!((held.head.epoch, asked), (1, vec![format!("put {key}")]));
        let behind = held.clone();
        for end in 1..=3 {
            let next = held.head.clone().holding(held.head.epoch, end);
            let (swapped, asked) = bucket.served(swap(&*bucket, CELL, &held, next));
            assert_eq!(asked, [format!("put {key}")]);
            held = swapped.unwrap();
        }
        let found = bucket.served(get(&*bucket, CELL)).0.unwrap().unwrap();
        assert_eq!((found.epoch, found.end), (1, 3));
        let stale = behind.head.clone().holding(behind.head.epoch, 99);
        let swapped = bucket.served(swap(&*bucket, CELL, &behind, stale)).0;
        assert_eq!(swapped.map(drop), Err(Error::Fenced));
        let keys: Vec<String> = bucket.keys().into_iter().collect();
        assert_eq!(keys, [key]);
    }

    /// Every swap writes bytes of its own, one more swap counted ([`Head::swaps`]): a change and
    /// its undoing leave the head as it was but for them, so that a write conditional on the ETag
    /// of the head of before, delayed, finds another (S3's ETags are of an object's bytes).
    #[test]
    fn swaps_back_to_what_a_head_held_write_other_bytes() {
        let bucket = SimBucket::new();
        bucket.served(crate::lifecycle::create(&*bucket, CELL, 7)).0.unwrap();
        let found = bucket.served(read(&*bucket, CELL)).0.unwrap();
        let held = bucket.served(take(&*bucket, CELL, found, &taking(1))).0.unwrap();
        let before = layout::encode(&held.head);
        let changed = Head { shredded: true, ..held.head.clone() };
        let held = bucket.served(swap(&*bucket, CELL, &held, changed)).0.unwrap();
        let back = Head { shredded: false, ..held.head.clone() };
        let held = bucket.served(swap(&*bucket, CELL, &held, back)).0.unwrap();
        assert_eq!(held.head.swaps, 3, "a taking and two swaps after the making");
        assert_ne!(layout::encode(&held.head), before);
    }

    /// The writer of a cell deleted and removed since, its head deleted, swaps conditional on it,
    /// which is no longer there: it fails, and creates nothing (spec rule `swapsInPlace`).
    #[test]
    fn writers_of_removed_cells_create_no_head() {
        let bucket = SimBucket::new();
        bucket.served(crate::lifecycle::create(&*bucket, CELL, 7)).0.unwrap();
        let found = bucket.served(read(&*bucket, CELL)).0.unwrap();
        let held = bucket.served(take(&*bucket, CELL, found, &taking(1))).0.unwrap();
        bucket.served(crate::lifecycle::delete(&*bucket, CELL, 2, 0)).0.unwrap();
        bucket.served(bucket::delete(&*bucket, &layout::head(CELL))).0.unwrap();
        let next = held.head.clone().holding(held.head.epoch, 5);
        let swapped = bucket.served(swap(&*bucket, CELL, &held, next)).0;
        assert_eq!(swapped.map(drop), Err(Error::Fenced));
        assert_eq!(bucket.served(get(&*bucket, CELL)).0, Ok(None));
    }

    /// A head of another format at the head's key is refused by name, never taken for none. (A
    /// bucket of 28's, its heads in slots, is refused by its identity's format first.)
    #[test]
    fn heads_of_other_formats_are_refused() {
        let bucket = SimBucket::new();
        let mut older = 28u32.to_le_bytes().to_vec();
        older.extend_from_slice(b"what format 28 held");
        bucket.served(bucket::create(&*bucket, &layout::head(CELL), older)).0.unwrap();
        let refused = Error::Format { cell: CELL, format: 28 };
        assert_eq!(bucket.served(get(&*bucket, CELL)).0, Err(refused.clone()));
        let found = bucket.served(read(&*bucket, CELL)).0.unwrap();
        let taken = bucket.served(take(&*bucket, CELL, found, &taking(1))).0;
        assert_eq!(taken.map(drop), Err(refused));
    }
}
