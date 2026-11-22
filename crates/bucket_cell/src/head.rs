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
//! cladefs: the head is N objects, its slots, N its bucket's setting
//! ([`crate::identity::Settings::head_slots`], TASK-691), as GCS takes about one write a second of
//! an object: version `k` of it, its `k`-th swap ([`Head::swaps`]), in slot `k % N`, each swap
//! conditional on its slot holding what its writer last saw there ([`Slots`]), so that only the
//! holder of the newest version writes the next. A read reads every slot and takes the newest, if
//! it found every slot, or a deletion's head, and no mark; else none: the head is being made, or
//! failed, or removed. A head is made with every slot, its maker creating them all ([`birth`]), and
//! no other write creates a version in one: a writer that held a cell removed since fails its next
//! swap, conditional on a slot no longer there, as it failed the one head's. A head found unmade,
//! its maker making it or dead making it, whoever needs it gone fails ([`fail`]): a mark in each
//! slot it found empty, so that the maker's creates there fail too, the head none for good; the
//! collector deletes the marks a day on ([`MARKS_KEPT`]), no request lasting so long.
//! specs/head-slots.

use crate::bucket::{self, Bucket, Created, Object, PutMode, Version};
use crate::cell::Kind;
use crate::keys::{Dek, InheritedDek, Kek};
use crate::layout::{self, CellId, Head, Replication, Wish};
use crate::lifecycle::{Start, Step};
use crate::{Error, rules};
use bytes::Bytes;
use std::collections::BTreeMap;

/// cladefs: how long the collector keeps a failed head's marks ([`fail`]), in milliseconds by the
/// backend's clock: a day, which no maker's request outlives, so that none of its creates lands
/// where a mark was (specs/head-slots' marksKept).
pub const MARKS_KEPT: u64 = 24 * 60 * 60 * 1000;

/// cladefs: how long the collector leaves a head unmade before it fails it ([`fail`]), in
/// milliseconds by the backend's clock, from its first slot's making: an hour, which a maker making
/// it takes far less of, so that a making is failed only once its maker is dead, or slower than any
/// is. Safety needs none of it: a making failed sooner fails as a fork given up on does.
pub const UNMADE_FOR: u64 = 60 * 60 * 1000;

/// cladefs: what the heads found in a head's slots, one each, say (TASK-691).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Found {
    /// The head: the newest, every slot holding one, or one a deletion's (rule `whole_or_final`).
    Head(Head),
    /// Its maker's version 0 in the first slot, a slot empty: being made, or its maker dead
    /// making it; to be failed by whoever needs it gone ([`fail`]).
    Unmade(Head),
    /// A mark among them: failed for good (rule `marks_fail`).
    Failed,
    /// None, or none found whole: being removed, or no head made.
    None,
}

/// cladefs: what `heads` say, the heads found in a head of `n` slots, one each (TASK-691): its
/// first slot's, if found, `first`.
pub(crate) fn found(n: u64, heads: impl IntoIterator<Item = Head>, first: Option<Head>) -> Found {
    let (mut count, mut deleted, mut marked, mut newest) = (0, false, false, None::<Head>);
    for head in heads {
        count += 1;
        deleted |= head.deleted();
        marked |= head.failed;
        if !head.failed && newest.as_ref().is_none_or(|newest| head.swaps > newest.swaps) {
            newest = Some(head);
        }
    }
    let Some(newest) = newest.filter(|_| !marked || !rules().marks_fail) else {
        return if marked { Found::Failed } else { Found::None };
    };
    let whole = match rules().slots_at_birth {
        true => count == n,
        // Without the rule, every slot to the newest's, the rest created as their turns come.
        false => count == (newest.swaps + 1).min(n),
    };
    if whole || deleted || !rules().whole_or_final {
        return Found::Head(newest);
    }
    match first {
        Some(first) if first.swaps == 0 && !first.failed => Found::Unmade(first),
        _ => Found::None,
    }
}

/// cladefs: a cell's head as a read of its slots, or a write of one, left it (TASK-691): its
/// bucket's count of slots; each slot's object's version, as last seen, which the next write of
/// that slot is conditional on; and what they say, the head among them.
#[derive(Clone, Debug)]
pub struct Slots {
    n: u64,
    /// The version of the object in each slot seen holding one.
    versions: BTreeMap<u64, Version>,
    /// What they say.
    found: Found,
    /// Why the head can't be told, if it can't: an object in a slot of another format.
    unreadable: Option<Error>,
}

impl Slots {
    /// The slots of a head of `n`, none holding anything: where a maker makes one.
    pub(crate) fn none(n: u64) -> Self {
        Slots { n, versions: BTreeMap::new(), found: Found::None, unreadable: None }
    }

    /// What `objects` say, the objects found in the slots of a head of `n`, by slot.
    fn of(cell: CellId, n: u64, objects: impl IntoIterator<Item = (u64, Object)>) -> Self {
        let mut slots = Slots::none(n);
        let (mut heads, mut first) = (Vec::new(), None);
        for (slot, object) in objects {
            slots.versions.insert(slot, object.meta.version.clone());
            match Head::decode(cell, &object.bytes) {
                Ok(head) => {
                    if slot == 0 {
                        first = Some(head.clone());
                    }
                    heads.push(head);
                }
                Err(error) => slots.unreadable = Some(error),
            }
        }
        slots.found = found(n, heads, first);
        slots
    }

    /// What the slots say.
    pub fn found(&self) -> &Found {
        &self.found
    }

    /// The head the slots hold, if they hold one: [`Error::Format`] if one is of another format
    /// than this one, whose objects this reads none of.
    pub fn head(&self, cell: CellId) -> Result<Option<Head>, Error> {
        let _ = cell;
        if let Some(error) = &self.unreadable {
            return Err(error.clone());
        }
        Ok(match &self.found {
            Found::Head(head) => Some(head.clone()),
            _ => None,
        })
    }

    /// The slots after a write of `head` in slot `slot`, of `version`.
    fn wrote(mut self, slot: u64, head: &Head, version: Version) -> Self {
        self.versions.insert(slot, version);
        self.found = Found::Head(head.clone());
        self
    }
}

/// A head as its writer last swapped it, or a taking took it: what it holds, and the slots its
/// next swap is conditional on.
#[derive(Clone, Debug)]
pub struct Held {
    pub head: Head,
    slots: Slots,
}

impl Held {
    /// The head as its write left it: what a later change of it starts from
    /// ([`crate::lifecycle::Start::Found`]), a read less.
    pub fn slots(&self) -> Slots {
        self.slots.clone()
    }

    /// cladefs: the key of the slot the next swap writes: what that swap waits for its backend to
    /// take ([`Bucket::rewritable`], TASK-690).
    pub fn next_key(&self, cell: CellId) -> String {
        layout::head(cell, (self.head.swaps + 1) % self.slots.n)
    }
}

/// `cell`'s head as a read finds it, its slots, for a taking of it after ([`take`]).
pub struct Read(pub(crate) Slots);

impl Read {
    /// What was found: the head as a change of it left it ([`Held::slots`], TASK-449), which a
    /// taking goes on from, its write conditional on it as on what a read found.
    pub fn found(slots: Slots) -> Self {
        Self(slots)
    }

    /// What was found, its slots: what a host learns for an opening of the cell after
    /// ([`crate::known::Learned`]).
    pub fn slots(self) -> Slots {
        self.0
    }

    /// The head read, if there was one.
    pub fn head(&self, cell: CellId) -> Result<Option<Head>, Error> {
        self.0.head(cell)
    }
}

/// Reads `cell`'s head for a taking of it after ([`take`]), as with other reads a taking needs,
/// at once (TASK-165.11): every slot, one GET each, at once (TASK-691).
/// cladefs: a slot's read refused, [`bucket::Refused`] (board 5861).
pub async fn read(bucket: &impl Bucket, cell: CellId) -> Result<Read, bucket::Refused> {
    let n = bucket.head_slots(cell).await;
    let gets = (0..n).map(|slot| async move {
        Ok::<_, bucket::Refused>(
            bucket::get(bucket, &layout::head(cell, slot)).await?.map(|object| (slot, object)),
        )
    });
    let objects: Vec<_> =
        futures::future::join_all(gets).await.into_iter().collect::<Result<_, _>>()?;
    Ok(Read(Slots::of(cell, n, objects.into_iter().flatten())))
}

/// `cell`'s head, if it has one: [`Error::Format`] if it is of another format than this one,
/// whose objects this reads none of.
pub async fn get(bucket: &impl Bucket, cell: CellId) -> Result<Option<Head>, Error> {
    read(bucket, cell).await?.head(cell)
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
    /// cladefs: what the cell's replication is to be (doc-21, TASK-537.3.1), its configuration
    /// kept: given whatever the head said, as the VM manager may change a cell's f or its fsync's
    /// level at any taking. None keeps what the head says.
    pub replication: Option<Wish>,
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
    let mut slots = found.0;
    loop {
        let Some(head) = slots.head(cell)? else { return Err(Error::NoCell) };
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
        let mut next = Head { epoch: head.epoch + 1, holder, token, kind, ..head.clone() };
        // cladefs: naming what it named, its versions among them (rule `versions_carried`,
        // TASK-476); without the rule, as a test shows it needed, none.
        if !rules().versions_carried {
            next.versions.clear();
        }
        if let Some(wish) = taking.replication {
            let configuration = next.replication.take().and_then(|had| had.configuration);
            next.replication = Some(Replication { wish, configuration });
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
        match write(bucket, cell, &slots, &head, next).await {
            Ok(held) => return Ok(held),
            Err(Unwritten::Changed) => slots = read(bucket, cell).await?.0,
            Err(Unwritten::Unmade) => return Err(Error::NoCell),
            Err(Unwritten::Refused(refused)) => return Err(refused.into()),
        }
    }
}

/// Swaps `cell`'s head from `held`, as its holder last wrote it, to `next`: with the condition
/// that its slot holds what that write left (rule `head_swap`), and so that taking still, which it
/// fails otherwise ([`Error::Fenced`]): another taking, or the cell's deletion or shredding,
/// swapped it since, or the collector removed it.
pub async fn swap(
    bucket: &impl Bucket,
    cell: CellId,
    held: &Held,
    next: Head,
) -> Result<Held, Error> {
    write(bucket, cell, &held.slots, &held.head, next).await.map_err(|unwritten| match unwritten {
        // cladefs: refused by the bucket, not fenced (board 5861).
        Unwritten::Refused(refused) => refused.into(),
        Unwritten::Changed | Unwritten::Unmade => Error::Fenced,
    })
}

/// Why a [`write`] wrote nothing.
#[derive(Debug)]
pub(crate) enum Unwritten {
    /// Another's write came first, or the slot is no longer there, removed: no write but its
    /// maker's creates a slot.
    Changed,
    /// The slot was never there: the head is its maker's, unmade, as a read without the rule
    /// `whole_or_final` takes it.
    Unmade,
    /// cladefs: the bucket refused the write, for good (board 5861).
    Refused(bucket::Refused),
}

impl From<bucket::Refused> for Unwritten {
    fn from(refused: bucket::Refused) -> Self {
        Self::Refused(refused)
    }
}

/// Writes `next`, the head after `head`, which `slots` hold: in the slot after `head`'s, its
/// `swaps` one more, conditional on that slot holding what `slots` saw there (rule `head_swap`;
/// without it, as a test shows it needed, whatever is there). A write that landed though its
/// answer was lost, its bytes read back, is its own.
async fn write(
    bucket: &impl Bucket,
    cell: CellId,
    slots: &Slots,
    head: &Head,
    next: Head,
) -> Result<Held, Unwritten> {
    let next = Head { swaps: head.swaps + 1, ..next };
    let slot = next.swaps % slots.n;
    let key = layout::head(cell, slot);
    let bytes = layout::encode(&next);
    let version = match rules().head_swap {
        true => match slots.versions.get(&slot) {
            Some(seen) => match bucket::update(bucket, &key, bytes, seen).await? {
                bucket::Updated::Updated(version) => version,
                bucket::Updated::Changed => return Err(Unwritten::Changed),
            },
            // A head read whole holds every slot (rule `slots_at_birth`); without the rule, as a
            // test shows it needed, the swap whose turn it is creates its slot.
            None if rules().slots_at_birth => return Err(Unwritten::Unmade),
            None => match bucket::create(bucket, &key, bytes).await? {
                Created::Created(version) => version,
                Created::Exists(_) => return Err(Unwritten::Changed),
            },
        },
        false => overwrite(bucket, &key, Bytes::from(bytes)).await?,
    };
    let slots = slots.clone().wrote(slot, &next, version);
    Ok(Held { head: next, slots })
}

/// Makes `cell`'s head `head`, its maker's (rule `slots_at_birth`): its first slot created, which
/// decides whose the cell is, then the rest, each a copy of it, one more swap each, their creates
/// at once. A first slot there already, another's, is what it fails with. Its own, a making of its
/// written before, it goes on making: a making's head's bytes are its own. A slot after the first
/// there already is a making of its own's too, or a mark of another's that failed the head, which
/// the slots it returns then say. Without the rule, as a test shows it needed, the first slot
/// alone: the others created by the swaps whose turns they are, so that a writer that held the
/// cell, removed since, creates its next slot, bringing back a live head.
/// cladefs: [`Unwritten::Changed`] for a first slot another's; refused, [`Unwritten::Refused`].
pub(crate) async fn birth(
    bucket: &impl Bucket,
    cell: CellId,
    head: Head,
) -> Result<Slots, Unwritten> {
    let n = bucket.head_slots(cell).await;
    let head = Head { swaps: 0, ..head };
    let first = layout::head(cell, 0);
    let version = match bucket::create(bucket, &first, layout::encode(&head)).await? {
        Created::Created(version) => version,
        Created::Exists(_) => return Err(Unwritten::Changed),
    };
    let mut objects = vec![(0, written(bucket, &first, &head, version))];
    if rules().slots_at_birth {
        let rest = (1..n).map(|slot| {
            let copy = Head { swaps: slot, ..head.clone() };
            async move {
                let key = layout::head(cell, slot);
                let object = match bucket::create(bucket, &key, layout::encode(&copy)).await? {
                    Created::Created(version) => written(bucket, &key, &copy, version),
                    Created::Exists(object) => object,
                };
                Ok::<_, bucket::Refused>((slot, object))
            }
        });
        let rest: Vec<_> =
            futures::future::join_all(rest).await.into_iter().collect::<Result<_, _>>()?;
        objects.extend(rest);
    }
    Ok(Slots::of(cell, n, objects))
}

/// cladefs: what [`fail`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failing {
    /// The head is failed, now or before: no head, for good.
    Failed,
    /// It was whole, or became so, its maker filling a slot first: a head to go on with.
    Whole,
    /// No head unmade: none at all, being removed, or another's, as `ours` said.
    Not,
}

/// cladefs: fails `cell`'s head, if a read finds it unmade and its maker's version 0 `ours`
/// (TASK-691, specs/head-slots): a mark created in each slot found empty, where nothing is, so that
/// its maker's creates there fail, and every read after finds no head, for good. A slot its maker
/// filled first ends it: read again, the head perhaps whole. No timing makes it safe: a maker slow
/// past it finds the marks where it creates; the collector deletes them a day on ([`MARKS_KEPT`]).
pub(crate) async fn fail(
    bucket: &impl Bucket,
    cell: CellId,
    ours: impl Fn(&Head) -> bool,
) -> Result<Failing, Error> {
    loop {
        let slots = read(bucket, cell).await?.0;
        if let Some(error) = slots.unreadable {
            return Err(error);
        }
        match slots.found {
            Found::Failed => return Ok(Failing::Failed),
            Found::Head(_) => return Ok(Failing::Whole),
            Found::None => return Ok(Failing::Not),
            Found::Unmade(first) if !ours(&first) => return Ok(Failing::Not),
            Found::Unmade(_) => {}
        }
        let mark = layout::encode(&Head { failed: true, ..Head::of(crate::record::Made::Own, 0) });
        let empty = (0..slots.n).filter(|slot| !slots.versions.contains_key(slot));
        let marked = empty.map(|slot| {
            let mark = mark.clone();
            async move {
                Ok::<_, bucket::Refused>(
                    match bucket::create(bucket, &layout::head(cell, slot), mark).await? {
                        Created::Created(_) => true,
                        Created::Exists(object) => {
                            Head::decode(cell, &object.bytes).is_ok_and(|found| found.failed)
                        }
                    },
                )
            }
        });
        let marked: Vec<bool> =
            futures::future::join_all(marked).await.into_iter().collect::<Result<_, _>>()?;
        if marked.into_iter().all(|marked| marked) {
            return Ok(Failing::Failed);
        }
    }
}

/// The object a write of `head` at `key` of `version` left.
fn written(bucket: &impl Bucket, key: &str, head: &Head, version: Version) -> Object {
    Object::written(key, layout::encode(head), version, bucket.now())
}

/// Changes `cell`'s head as `step` says of what is there, starting from `start`, until it writes
/// nothing or what it writes lands, read again whenever another's write came first: a head made
/// whole ([`birth`]) where none is, the next swap of one there ([`write`]). Fails with
/// [`Error::Unmade`] for a head found being made, or failed, or removed, which no one but its maker
/// writes ([`fail`] fails it). With the slots it left.
pub(crate) async fn change_leaving<R>(
    bucket: &impl Bucket,
    cell: CellId,
    start: Start<Slots>,
    mut step: impl FnMut(Option<Head>) -> Result<Step<Head, R>, Error>,
) -> Result<(R, Slots), Error> {
    let mut slots = match start {
        Start::Read => read(bucket, cell).await?.0,
        Start::Create => Slots::none(bucket.head_slots(cell).await),
        Start::Found(found) => found,
    };
    loop {
        let current = slots.head(cell)?;
        if current.is_none() && !slots.versions.is_empty() {
            return Err(Error::Unmade(cell));
        }
        let (next, result) = match step(current.clone())? {
            Step::Stop(result) => return Ok((result, slots)),
            Step::Write(next, result) => (next, result),
        };
        let written = match &current {
            None => birth(bucket, cell, next).await,
            Some(head) => write(bucket, cell, &slots, head, next).await.map(|held| held.slots),
        };
        match written {
            Ok(written) => return Ok((result, written)),
            Err(Unwritten::Changed) => slots = read(bucket, cell).await?.0,
            Err(Unwritten::Unmade) => return Err(Error::Unmade(cell)),
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
            Err(_) => {
                failures += 1;
                bucket.pause(failures).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::test_cell;
    use crate::record::Made;
    use crate::testing::{Direct, SimBucket};
    use futures::executor::block_on;
    use std::sync::Arc;

    const CELL: CellId = test_cell(3);
    const N: u64 = crate::identity::DEFAULT_HEAD_SLOTS;

    /// The keys of `cell`'s head's slots that hold something, and the swaps each holds.
    fn slots(bucket: &impl Bucket, cell: CellId) -> Vec<(u64, u64)> {
        let listed = block_on(bucket::list(bucket, &layout::heads(cell))).unwrap();
        let swaps = |meta: &bucket::Meta| {
            let object = block_on(bucket::get(bucket, &meta.key)).unwrap()?;
            let layout::Kind::Head { slot, .. } = layout::kind(&meta.key) else { return None };
            Some((slot, Head::decode(cell, &object.bytes).unwrap().swaps))
        };
        listed.iter().filter_map(swaps).collect()
    }

    fn taking(holder: u64) -> Taking {
        Taking { holder, ..Taking::default() }
    }

    /// TASK-691: a head is made with every slot, versions 0 to N-1, each a copy of its maker's; a
    /// taking and each swap after write the slot after the newest's, conditional on what was there,
    /// round and round; a read finds the newest. A writer behind fails its swap, as with the one
    /// head.
    #[test]
    fn heads_are_made_whole_and_swapped_in_turn() {
        let bucket = Direct(SimBucket::new());
        assert_eq!(block_on(crate::lifecycle::create(&bucket, CELL, 7)), Ok(true));
        let made: Vec<_> = (0..N).map(|slot| (slot, slot)).collect();
        assert_eq!(slots(&bucket, CELL), made);
        let head = block_on(get(&bucket, CELL)).unwrap().unwrap();
        assert_eq!((head.owner, head.swaps, head.epoch), (7, N - 1, 0));

        let mut held =
            block_on(take(&bucket, CELL, block_on(read(&bucket, CELL)).unwrap(), &taking(1)))
                .unwrap();
        assert_eq!((held.head.swaps, held.head.epoch), (N, 1));
        let behind = held.clone();
        for end in 1..=N + 2 {
            let next = Head { end, ..held.head.clone() };
            held = block_on(swap(&bucket, CELL, &held, next)).unwrap();
        }
        assert_eq!(held.head.swaps, 2 * N + 2);
        let found = block_on(get(&bucket, CELL)).unwrap().unwrap();
        assert_eq!((found.swaps, found.end), (2 * N + 2, N + 2));
        let listed = slots(&bucket, CELL);
        assert_eq!(listed.len() as u64, N);
        assert_eq!(listed[2], (2, 2 * N + 2));
        assert_eq!(listed[3], (3, N + 3));
        let stale = Head { end: 99, ..behind.head.clone() };
        assert_eq!(block_on(swap(&bucket, CELL, &behind, stale)).map(drop), Err(Error::Fenced));
        assert_eq!(block_on(get(&bucket, CELL)).unwrap().unwrap().end, N + 2);
    }

    /// The writer of a cell deleted and removed since, every slot deleted, swaps conditional on its
    /// slot, which no longer is: it fails, and creates nothing (specs/head-slots' DeletedStays).
    fn swap_after_removal() -> (Result<(), Error>, Option<Head>) {
        let bucket = Direct(SimBucket::new());
        block_on(crate::lifecycle::create(&bucket, CELL, 7)).unwrap();
        let held =
            block_on(take(&bucket, CELL, block_on(read(&bucket, CELL)).unwrap(), &taking(1)))
                .unwrap();
        block_on(crate::lifecycle::delete(&bucket, CELL, 2, 0)).unwrap();
        for key in block_on(bucket::list(&bucket, &layout::heads(CELL))).unwrap() {
            block_on(bucket::delete(&bucket, &key.key)).unwrap();
        }
        let next = Head { end: 5, ..held.head.clone() };
        let swapped = block_on(swap(&bucket, CELL, &held, next)).map(drop);
        let found = block_on(bucket::get(&bucket, &held.next_key(CELL)))
            .unwrap()
            .map(|object| Head::decode(CELL, &object.bytes).unwrap());
        (swapped, found)
    }

    #[test]
    fn writers_of_removed_cells_create_no_slot() {
        assert_eq!(swap_after_removal(), (Err(Error::Fenced), None));
    }

    /// Without slots_at_birth, a head made in its first slot alone, the others created as their
    /// turns come: the writer of the removed cell creates its next slot, and a live head is back.
    #[test]
    fn writers_of_removed_cells_create_no_slot_need_slots_at_birth() {
        crate::set_rules(crate::Rules { slots_at_birth: false, ..crate::Rules::ALL });
        let (swapped, found) = swap_after_removal();
        crate::set_rules(crate::Rules::ALL);
        assert_eq!(swapped, Ok(()));
        assert_eq!(found.map(|head| (head.end, head.deleted())), Some((5, false)));
    }

    /// A head whose maker made its first slot alone, dying before the rest, or still making them,
    /// is none to a reader (rule `whole_or_final`), and to a change of it, which no one but its
    /// maker makes ([`Error::Unmade`]); a taking finds no cell. Without the rule, the first slot's
    /// head is read, and taken, a head its maker is still making.
    fn unmade() -> (Result<Option<Head>, Error>, Result<u64, Error>) {
        let bucket = Direct(SimBucket::new());
        let first = Head::of(Made::Root, 7);
        block_on(bucket::create(&bucket, &layout::head(CELL, 0), layout::encode(&first))).unwrap();
        let found = block_on(get(&bucket, CELL));
        let taken =
            block_on(take(&bucket, CELL, block_on(read(&bucket, CELL)).unwrap(), &taking(1)));
        (found, taken.map(|held| held.head.epoch))
    }

    #[test]
    fn heads_being_made_are_none() {
        assert_eq!(unmade(), (Ok(None), Err(Error::NoCell)));
        let bucket = Direct(SimBucket::new());
        let first = Head::of(Made::Root, 7);
        block_on(bucket::create(&bucket, &layout::head(CELL, 0), layout::encode(&first))).unwrap();
        // Deleted, it is failed: a mark in each slot after the first.
        let deleted = block_on(crate::lifecycle::delete(&bucket, CELL, 2, 0));
        assert_eq!(deleted, Ok(()));
        assert_eq!(slots(&bucket, CELL).len() as u64, N);
        assert_eq!(*block_on(read(&bucket, CELL)).unwrap().slots().found(), Found::Failed);
        assert_eq!(block_on(crate::lifecycle::create(&bucket, CELL, 8)), Ok(false));
    }

    #[test]
    fn heads_being_made_are_none_need_whole_or_final() {
        crate::set_rules(crate::Rules { whole_or_final: false, ..crate::Rules::ALL });
        let (found, taken) = unmade();
        crate::set_rules(crate::Rules::ALL);
        assert_eq!(found.map(|head| head.map(|head| head.owner)), Ok(Some(7)));
        // Its swap's slot never made, it fails, though: the maker's to create.
        assert_eq!(taken, Err(Error::NoCell));
    }

    /// A head whose maker made its first slot alone, dying, or still making it, is failed by the
    /// deletion of it, a mark in each empty slot: no head for good, a deletion after finding it
    /// gone; the maker's create of another slot after fails, finding the mark. A deletion of the
    /// fork's of another owner leaves it (the simulation's forks, whose failed children were left
    /// so).
    #[test]
    fn unmade_heads_are_failed_by_their_deletions() {
        let bucket = Direct(SimBucket::new());
        let origin = layout::Origin { parent: test_cell(1), at: 3 };
        let forking = Head::of(Made::Forking { origin, version: 9 }, 0x33);
        block_on(bucket::create(&bucket, &layout::head(CELL, 0), layout::encode(&forking)))
            .unwrap();
        let unfork = |owner| block_on(crate::lifecycle::delete_fork(&bucket, CELL, owner, (2, 0)));
        assert_eq!(unfork(0x44), Ok(false));
        assert_eq!(slots(&bucket, CELL), [(0, 0)]);
        assert_eq!(unfork(0x33), Ok(true));
        assert_eq!(block_on(get(&bucket, CELL)), Ok(None));
        assert_eq!(*block_on(read(&bucket, CELL)).unwrap().slots().found(), Found::Failed);
        let copy = layout::encode(&Head { swaps: 1, ..forking });
        let late = block_on(bucket::create(&bucket, &layout::head(CELL, 1), copy)).unwrap();
        assert!(matches!(late, Created::Exists(_)), "the maker's late create landed");
        assert_eq!(unfork(0x33), Ok(true), "gone already");
        assert_eq!(block_on(crate::lifecycle::delete(&bucket, CELL, 2, 0)), Ok(()));
        assert_eq!(block_on(get(&bucket, CELL)), Ok(None));
    }

    /// Without marks_fail, a failed head reads as its maker's version 0, a head that never was,
    /// every slot holding something (specs/head-slots' NoMarksFail).
    #[test]
    fn unmade_heads_are_failed_by_their_deletions_need_marks_fail() {
        let bucket = Direct(SimBucket::new());
        let made = Head::of(Made::Root, 7);
        block_on(bucket::create(&bucket, &layout::head(CELL, 0), layout::encode(&made))).unwrap();
        assert_eq!(block_on(fail(&bucket, CELL, |_| true)), Ok(Failing::Failed));
        crate::set_rules(crate::Rules { marks_fail: false, ..crate::Rules::ALL });
        let found = block_on(get(&bucket, CELL));
        crate::set_rules(crate::Rules::ALL);
        assert_eq!(found.map(|head| head.map(|head| (head.owner, head.swaps))), Ok(Some((7, 0))));
    }

    /// TASK-691: a bucket formatted with 3 slots a head keeps its heads in 3, made, read and
    /// swapped round them; a bucket of no identity, the default's.
    #[test]
    fn heads_are_kept_in_their_buckets_count_of_slots() {
        let memory = Arc::new(object_store::memory::InMemory::new());
        let bucket = crate::object_bucket::ObjectBucket::new(memory, "");
        let three = crate::identity::Asked { head_slots: Some(3) };
        block_on(crate::identity::format(&bucket, &Default::default(), three)).unwrap();
        block_on(crate::lifecycle::create(&bucket, CELL, 7)).unwrap();
        assert_eq!(slots(&bucket, CELL), [(0, 0), (1, 1), (2, 2)]);
        let held =
            block_on(take(&bucket, CELL, block_on(read(&bucket, CELL)).unwrap(), &taking(1)));
        assert_eq!(held.map(|held| held.next_key(CELL)), Ok(layout::head(CELL, 1)));
        assert_eq!(slots(&bucket, CELL), [(0, 3), (1, 1), (2, 2)]);
        let unformatted = Direct(SimBucket::new());
        assert_eq!(block_on(unformatted.head_slots(CELL)), N);
    }

    /// A deletion's head is found whatever slots are left, as the collector removes them; none once
    /// they all are.
    #[test]
    fn deleted_heads_are_found_as_their_slots_go() {
        let bucket = Direct(SimBucket::new());
        block_on(crate::lifecycle::create(&bucket, CELL, 7)).unwrap();
        block_on(crate::lifecycle::delete(&bucket, CELL, 2, 0)).unwrap();
        // The deletion's, the slot after the maker's last.
        let deletion = layout::head(CELL, 0);
        for meta in block_on(bucket::list(&bucket, &layout::heads(CELL))).unwrap() {
            if meta.key != deletion {
                block_on(bucket::delete(&bucket, &meta.key)).unwrap();
                let found = block_on(get(&bucket, CELL)).unwrap().unwrap();
                assert!(found.deleted(), "{found:?}");
            }
        }
        block_on(bucket::delete(&bucket, &deletion)).unwrap();
        assert_eq!(block_on(get(&bucket, CELL)), Ok(None));
    }
}
