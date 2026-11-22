// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Cells' lives, as their heads say them ([`Head`], TASK-515): creating a cell, forking one,
//! deleting one. A taking is [`crate::head::take`]'s.
//!
//! A fork is of a version: its child's head alone, which writes nothing of the version's cell's,
//! so that any number of hosts fork one at once ([`fork_version`]).
//!
//! cladefs: every change of a head but a taking's and its writer's swaps, and of a collector lease,
//! is a [`transition`], one compare-and-swap loop, of what it found to what it writes
//! (TASK-160.14).

use crate::bucket::{self, Bucket, Created, Object, Updated};
use crate::head::{self, Slots};
use crate::keys::Parents;
use crate::layout::{self, CellId, Head, Origin};
use crate::record::Made;
use crate::recovery;
use crate::{Error, rules};
use futures::StreamExt as _;
use serde::Serialize;

/// What a [`transition`] makes of the object it found.
pub(crate) enum Step<T, R> {
    /// Writes `T` in place of what it found, if that is still there, and gives `R`.
    Write(T, R),
    /// Writes nothing, and gives `R`.
    Stop(R),
}

/// What a [`transition`] starts from: of an object, or of a cell's head, its slots
/// ([`head::Slots`], TASK-691).
pub enum Start<F = Option<Object>> {
    /// What a read finds.
    Read,
    /// Nothing, as a create expects: one that finds something there reads it from its answer.
    Create,
    /// What a read before found: a write after it finds the object changed, if another's write
    /// came between, and reads it again, as it does after its own read.
    Found(F),
}

/// Changes the object at `key`, which `decode` reads, as `step` says of what is there, if anything,
/// starting from `start`: by a create where nothing is, and a conditional update otherwise, given
/// to `step` again, from what is there then, whenever another's write came first, until it writes
/// nothing or what it writes lands. A write of it that landed though its answer was lost is taken
/// for its own ([`bucket::create`], [`bucket::update`]), so that what `step` writes must differ
/// from every other writer's: a head names its owner and holder, a lease its holder.
pub(crate) async fn transition<T: Serialize, R>(
    bucket: &impl Bucket,
    key: &str,
    start: Start,
    decode: impl Fn(&[u8]) -> Result<T, Error>,
    step: impl FnMut(Option<T>) -> Result<Step<T, R>, Error>,
) -> Result<R, Error> {
    Ok(transition_leaving(bucket, key, start, decode, step).await?.0)
}

/// [`transition`], with the object it left at `key`: what it wrote, of the version its write
/// made, or what it found and stopped at, none if nothing was there (TASK-449). A later
/// transition of the object starts from it ([`Start::Found`]), a read less, its write conditional
/// on that version as on one a read found.
pub(crate) async fn transition_leaving<T: Serialize, R>(
    bucket: &impl Bucket,
    key: &str,
    start: Start,
    decode: impl Fn(&[u8]) -> Result<T, Error>,
    mut step: impl FnMut(Option<T>) -> Result<Step<T, R>, Error>,
) -> Result<(R, Option<Object>), Error> {
    let mut found = match start {
        Start::Read => bucket::get(bucket, key).await?,
        Start::Create => None,
        Start::Found(found) => found,
    };
    loop {
        let current = found.as_ref().map(|object| decode(&object.bytes)).transpose()?;
        let (next, result) = match step(current)? {
            Step::Stop(result) => return Ok((result, found)),
            Step::Write(next, result) => (next, result),
        };
        let bytes = layout::encode(&next);
        let written = |version| Object::written(key, bytes.clone(), version, bucket.now());
        found = match found {
            None => match bucket::create(bucket, key, bytes.clone()).await? {
                Created::Created(version) => return Ok((result, Some(written(version)))),
                Created::Exists(object) => Some(object),
            },
            Some(object) => {
                match bucket::update(bucket, key, bytes.clone(), &object.meta.version).await? {
                    Updated::Updated(version) => return Ok((result, Some(written(version)))),
                    Updated::Changed => bucket::get(bucket, key).await?,
                }
            }
        };
    }
}

/// Changes `cell`'s head as `step` says ([`head::change_leaving`]).
pub(crate) async fn change<R>(
    bucket: &impl Bucket,
    cell: CellId,
    start: Start<Slots>,
    step: impl FnMut(Option<Head>) -> Result<Step<Head, R>, Error>,
) -> Result<R, Error> {
    Ok(change_leaving(bucket, cell, start, step).await?.0)
}

/// Changes `cell`'s head as [`change`] does, with the slots it left.
async fn change_leaving<R>(
    bucket: &impl Bucket,
    cell: CellId,
    start: Start<Slots>,
    step: impl FnMut(Option<Head>) -> Result<Step<Head, R>, Error>,
) -> Result<(R, Slots), Error> {
    head::change_leaving(bucket, cell, start, step).await
}

/// Creates `cell`, of no parent, of `owner`, which its maker chose at random, never taken: its
/// first taking gives it its keys and kind ([`head::take`]). Returns whether it made it: a head
/// there already, whoever's, being made or not, is left as it is.
pub async fn create(bucket: &impl Bucket, cell: CellId, owner: u64) -> Result<bool, Error> {
    let head = Head::of(Made::Root, owner);
    let made = change(bucket, cell, Start::Create, |found| match found {
        None => Ok(Step::Write(head.clone(), true)),
        Some(_) => Ok(Step::Stop(false)),
    })
    .await;
    match made {
        Err(Error::Unmade(_)) => Ok(false),
        made => made,
    }
}

/// cladefs: writes the reference of the fork of `origin` to `child`, whose owner is `owner`, under
/// the parent's cell ([`layout::reference`], TASK-395.3): create-only, its bytes the fork's alone
/// ([`layout::Reference`]), so that every writer of it, and a write landed though its answer was
/// lost, writes the same. One there already stands, whatever its bytes: a pass keeps by its key.
///
/// cladefs: in the child's bucket, if the version's mirror is marked there; else in the parent's
/// (doc-23, Forks; TASK-633.8). Where it went.
async fn refer(
    bucket: &impl Bucket,
    origin: Origin,
    child: CellId,
    owner: u64,
) -> Result<Referred, bucket::Refused> {
    let key = layout::reference(origin.parent, origin.at, child);
    let bytes = layout::encode(&layout::Reference::of(owner));
    if let Some(here) = bucket.mirrors()
        && here.marked(origin.parent, origin.at).await?
    {
        here.create(&key, bytes).await?;
        return Ok(Referred { owner, here: true });
    }
    bucket::create(bucket, &key, bytes).await?;
    Ok(Referred { owner, here: false })
}

/// cladefs: where a fork's reference went ([`refer`]): `here`, in the child's bucket, its version's
/// mirror marked there; else in the parent's.
#[derive(Clone, Copy, Debug)]
struct Referred {
    owner: u64,
    here: bool,
}

/// cladefs: what a fork that `referred` does, once it read its version named in the parent's head,
/// to leave the parent's bucket (doc-23, Forks; TASK-633.8): referring there, the version's mirror
/// marked in the child's bucket since, read again (`forksRecheck`), its reference written here
/// too; then, the head read again naming the version still (`forksRereadBeforeLeaving`), its own
/// there retired (`forksLeaveA`), which no mirror's listing may have found, and which would keep
/// the version there as long as the child lives. Read again, the head says that the mirror's
/// bucket's collector, which reads it before it lists the references here, keeps the copy, or
/// finds the reference here: a version dropped by then, the reference there stays.
///
/// A fork made again after one of its makings referred there and stopped, referring here this
/// time, leaves that reference there: a making begun again writes its head's bytes as the first
/// did, and can't tell it from its own write whose answer was lost. The mirror's hold keeps the
/// version there as long, and the child's deletion retires it.
async fn leave_parents(
    bucket: &impl Bucket,
    origin: Origin,
    version: u64,
    child: CellId,
    referred: Referred,
) -> Result<(), Error> {
    let Referred { owner, here: false } = referred else { return Ok(()) };
    let Some(here) = bucket.mirrors() else { return Ok(()) };
    if !here.marked(origin.parent, origin.at).await? {
        return Ok(());
    }
    let key = layout::reference(origin.parent, origin.at, child);
    here.create(&key, layout::encode(&layout::Reference::of(owner))).await?;
    failpoint!("version::rechecked");
    let head = head::get(bucket, origin.parent).await?;
    let named = head.as_ref().filter(|head| !head.deleted()).and_then(|head| head.version(version));
    if named.is_some_and(|named| named.at == origin.at) {
        retire_one(bucket, &key).await?;
    }
    Ok(())
}

/// cladefs: retires the references at `keys` ([`layout::reference`]), garbage for good, under any
/// cells: marks each ([`layout::retired`]), then deletes them, many at once (TASK-395.13,
/// TASK-395.14). One a writer whose read of its child's head came before writes again after, the
/// pass over its parent's cell deletes, by its mark. Without the rule, as a test shows it needed,
/// deleted alone.
pub(crate) async fn retire(bucket: &impl Bucket, keys: &[&str]) -> Result<(), bucket::Refused> {
    if rules().retirements_marked {
        let marks: Vec<String> = keys.iter().filter_map(|key| layout::retired_of(key)).collect();
        // The creates' futures made first, as `bucket::delete_all`'s batches are: a stream mapping
        // them in holds the closure in its type, which a future awaiting this one is then not seen
        // to be `Send` with.
        let marking: Vec<_> = marks.iter().map(|mark| mark_retired(bucket, mark)).collect();
        let marked: Vec<Result<(), bucket::Refused>> =
            futures::stream::iter(marking).buffer_unordered(MARKS_AT_ONCE).collect().await;
        marked.into_iter().collect::<Result<(), _>>()?;
    }
    bucket::delete_all(bucket, keys).await
}

/// How many references' marks [`retire`] writes at once.
const MARKS_AT_ONCE: usize = 16;

/// Writes the mark `key` ([`layout::retired`]): there already, it stands, its bytes everyone's.
async fn mark_retired(bucket: &impl Bucket, key: &str) -> Result<(), bucket::Refused> {
    bucket::create(bucket, key, layout::encode(&layout::Retired::NOW)).await.map(drop)
}

/// [`retire`]s the reference at `key` alone, a request a step, with no stream of batches, so that a
/// writer's future, which the uploader's runtime needs `Send`, may await it (TASK-395.10).
async fn retire_one(bucket: &impl Bucket, key: &str) -> Result<(), bucket::Refused> {
    if rules().retirements_marked
        && let Some(mark) = layout::retired_of(key)
    {
        mark_retired(bucket, &mark).await?;
    }
    bucket::delete(bucket, key).await
}

/// cladefs: retires the reference of the fork of `origin` to `child`, whose head, `found`, names
/// another origin than the fork's, or none (TASK-395.10): the fork made no cell, and no head will
/// name it, so that no pass would retire it ([`crate::collector`] retires a reference by the origin
/// its child's deleted head names). Marked ([`retire`]).
async fn take_back(
    bucket: &impl Bucket,
    origin: Origin,
    child: CellId,
    found: &Head,
) -> Result<(), bucket::Refused> {
    if rules().references && found.origin() != Some(origin) {
        retire_one(bucket, &layout::reference(origin.parent, origin.at, child)).await?;
    }
    Ok(())
}

/// cladefs: a fork of the version `version` of the cell `origin` names, at the segment it names, to
/// `child`, an ephemeral cell kept on the host of incarnation `incarnation` alone (doc-25,
/// TASK-698.3; specs/ephemeral): its reference's index entry, then its reference, written before
/// the cell's head is read (`forksReferFirst`), which must name the version there
/// ([`Error::Deleted`] otherwise), shared if `child` is another tenant's ([`Error::NotShared`]);
/// refused, or the head not to be read, its reference goes again. Then the cell's checkpoint at the
/// version's segment, made if none is there, sealed under `keys`, as a fork into another cell's is
/// (`recovery::checkpoint_at`): the child's openings recover its parent's state from there, one
/// decoded state for all of them (TASK-698.8), not from an earlier one, replaying the segments
/// between each time. No head of the child's: its host keeps its state. A host crashing part way
/// leaves the reference, which it retires starting again, or its decommissioning does.
pub async fn fork_ephemeral(
    bucket: &impl Bucket,
    child: CellId,
    version: u64,
    origin: Origin,
    (incarnation, keys): (u64, Parents<'_>),
) -> Result<(), Error> {
    refer_ephemeral(bucket, origin, incarnation, child).await?;
    failpoint!("ephemeral::referred");
    let cell = origin.parent;
    let crosses = cell.tenant != child.tenant && cell.tenant != layout::DEFAULT_TENANT;
    let refused = match head::get(bucket, cell).await {
        Err(error) => Some(error),
        Ok(head) => {
            let named = head.as_ref().and_then(|head| head.version(version));
            match named.filter(|named| named.at == origin.at && named.epoch == origin.epoch) {
                None => Some(Error::Deleted),
                Some(named) if crosses && !named.shared => Some(Error::NotShared(cell)),
                Some(_) => None,
            }
        }
    };
    let refused = match refused {
        None => recovery::checkpoint_at(bucket, cell, origin.at, keys).await.err(),
        refused => refused,
    };
    match refused {
        None => Ok(()),
        Some(error) => {
            // cladefs: the fork's failure what it fails with, whatever undoing it came to.
            if let Err(refused) = unrefer_ephemeral(bucket, origin, incarnation, child).await {
                log::warn!("cladefs: undoing the ephemeral fork {child}: {refused}");
            }
            Err(error)
        }
    }
}

/// cladefs: writes the reference of `child`, an ephemeral cell of the host of incarnation
/// `incarnation`, to `origin` ([`layout::ephemeral_reference`]), its index entry first
/// ([`layout::ephemeral_index`]), so that decommissioning the incarnation finds every reference
/// of its.
pub async fn refer_ephemeral(
    bucket: &impl Bucket,
    origin: Origin,
    incarnation: u64,
    child: CellId,
) -> Result<(), bucket::Refused> {
    let index = layout::ephemeral_index(incarnation, origin.parent, origin.at, child);
    bucket::create(bucket, &index, Vec::new()).await?;
    let reference = layout::ephemeral_reference(origin.parent, origin.at, incarnation, child);
    bucket::create(bucket, &reference, Vec::new()).await.map(drop)
}

/// cladefs: deletes the reference of `child`, an ephemeral cell of the host of incarnation
/// `incarnation`, to `origin`, then its index entry: its cell gone, or never made, or its host
/// gone for good ([`decommission`]).
pub async fn unrefer_ephemeral(
    bucket: &impl Bucket,
    origin: Origin,
    incarnation: u64,
    child: CellId,
) -> Result<(), bucket::Refused> {
    let reference = layout::ephemeral_reference(origin.parent, origin.at, incarnation, child);
    bucket::delete(bucket, &reference).await?;
    let index = layout::ephemeral_index(incarnation, origin.parent, origin.at, child);
    bucket::delete(bucket, &index).await
}

/// cladefs: the references of the ephemeral cells of the host of incarnation `incarnation`, by
/// their index (doc-25): each child, with its origin.
/// cladefs: refused, [`bucket::Refused`] (board 5861).
pub async fn ephemeral_references(
    bucket: &impl Bucket,
    incarnation: u64,
) -> Result<Vec<(Origin, CellId)>, bucket::Refused> {
    let listed = bucket::list(bucket, &layout::ephemeral_indexes(incarnation)).await?;
    Ok((listed.iter())
        .filter_map(|meta| layout::ephemeral_indexed(&meta.key))
        .filter(|&(of, ..)| of == incarnation)
        .map(|(_, parent, at, child)| (Origin { parent, at, epoch: 0 }, child))
        .collect())
}

/// cladefs: decommissions the host of incarnation `incarnation`, gone for good, its disk lost
/// (doc-25, TASK-698.3; specs/ephemeral): the references of its ephemeral cells, which no host
/// holds any more, deleted, each with its index entry. Returns how many. Its operator's to call,
/// for an incarnation no host has: a host of it still holding its cells would read what it no
/// longer keeps.
pub async fn decommission(
    bucket: &impl Bucket,
    incarnation: u64,
) -> Result<usize, bucket::Refused> {
    let references = ephemeral_references(bucket, incarnation).await?;
    for &(origin, child) in &references {
        unrefer_ephemeral(bucket, origin, incarnation, child).await?;
    }
    Ok(references.len())
}

/// cladefs: a restore of the version `version` of the cell `origin` names, at the segment it
/// names, to `child`, whose owner is `owner` (doc-18, TASK-476): the child's head made, being
/// made ([`Made::Forking`]), then its reference to its origin, then the cell's head read, which
/// must name the version there ([`Error::Deleted`] otherwise, the version dropped or the cell
/// deleted), shared if `child` is of another tenant's cell ([`Error::NotShared`]), then the fork
/// made, a clone ([`Made::Clone`]). Forked again, its head being made, it resumes the fork as
/// another would ([`resume_version_fork`]). For a child of another cell than the cell's, the
/// cell's checkpoint at the version's segment too, before the fork is made, made if none is
/// there, sealed under `keys` (`recovery::checkpoint_at`): the child's recovery reads its parent
/// across cells by that key alone, and only the first restore into another cell writes it.
pub async fn fork_version(
    bucket: &impl Bucket,
    child: CellId,
    version: u64,
    origin: Origin,
    owner: u64,
    keys: Parents<'_>,
) -> Result<(), Error> {
    fork_version_keyed(bucket, child, version, origin, (owner, None), keys).await.map(drop)
}

/// cladefs: [`fork_version`], the child's head made with `given`, if any: a KEK for the child's
/// cell, its first DEK and those it inherits, rewrapped under that KEK ([`crate::keys::giving`],
/// TASK-411.4), so that its first taking gives it none.
pub async fn fork_version_keyed(
    bucket: &impl Bucket,
    child: CellId,
    version: u64,
    origin: Origin,
    (owner, given): (u64, Option<head::Given>),
    keys: Parents<'_>,
) -> Result<(), Error> {
    failpoint!("version::record");
    let refers = rules().references && rules().clones_refer_first;
    // Without the rule, as a test shows it needed: the reference before the head.
    let refers_first = refers && !rules().clones_record_first;
    let mut referred = None;
    if refers_first {
        referred = Some(refer(bucket, origin, child, owner).await?);
        failpoint!("version::referred");
    }
    let made = match rules().forks_pending {
        true => Made::Forking { origin, version },
        false => Made::Clone(origin),
    };
    let mut head = Head::of(made, owner);
    if let Some(given) = given {
        head.keks.extend(given.kek);
        (head.deks, head.inherited) = given.deks.unwrap_or_default();
    }
    // A head found that refused the fork.
    let mut refused = None;
    let making = change_leaving(bucket, child, Start::Create, |found| match found {
        None => Ok(Step::Write(head.clone(), true)),
        // Another's, deleted or not: its ID taken, as a read of the head before found.
        Some(found) if !found.is_child_of(head.owner) => {
            refused = Some(found);
            Err(Error::IdTaken(child))
        }
        // Ours, given up on, its version dropped.
        Some(found) if found.deleted() => {
            refused = Some(found);
            Err(Error::Deleted)
        }
        Some(found) => Ok(Step::Stop(found.forking().is_some())),
    })
    .await;
    if let Some(found) = refused.filter(|_| refers_first) {
        take_back(bucket, origin, child, &found).await?;
    }
    // cladefs: a head another making is making, or its maker died making, the ID is taken
    // (TASK-691).
    let (making, left) = making.map_err(|error| match error {
        Error::Unmade(_) => Error::IdTaken(child),
        error => error,
    })?;
    if making {
        failpoint!("version::recorded");
        if refers && !refers_first {
            referred = Some(refer(bucket, origin, child, owner).await?);
            failpoint!("version::referred");
        }
        let start = Start::Found(left);
        finish_fork(bucket, child, version, origin, keys, start, referred).await?;
    }
    // Without the rule, as a test shows it needed: once made.
    if rules().references && !rules().clones_refer_first {
        refer(bucket, origin, child, owner).await?;
    }
    Ok(())
}

/// The fork being made of the version `version`, of `origin`, to `child`, made, the child's head's
/// update starting from `start`: the head as its create left it, a read less, its update
/// conditional on that version as on one a read found, so that its deletion or its giving up since
/// makes it read the head anew. The version's cell's head read first, and for a child of another
/// cell, the cell's checkpoint at the version made, sealed under `keys` (TASK-476).
///
/// cladefs: having `referred`, what leaves the parent's bucket, after the head's read
/// ([`leave_parents`]).
async fn finish_fork(
    bucket: &impl Bucket,
    child: CellId,
    version: u64,
    origin: Origin,
    keys: Parents<'_>,
    start: Start<Slots>,
    referred: Option<Referred>,
) -> Result<(), Error> {
    let cell = origin.parent;
    // Into another tenant's cell, a version of the default tenant's, or one shared.
    let crosses = cell.tenant != child.tenant && cell.tenant != layout::DEFAULT_TENANT;
    if rules().forks_check_versions || crosses {
        let head = head::get(bucket, cell).await?;
        let named = head.as_ref().and_then(|head| head.version(version));
        let named = named.filter(|named| named.at == origin.at && named.epoch == origin.epoch);
        if rules().forks_check_versions && named.is_none() {
            return Err(Error::Deleted);
        }
        if crosses && !named.is_some_and(|named| named.shared) {
            return Err(Error::NotShared(cell));
        }
    }
    if let Some(referred) = referred {
        leave_parents(bucket, origin, version, child, referred).await?;
    }
    if cell != child {
        recovery::checkpoint_at(bucket, cell, origin.at, keys).await?;
    }
    failpoint!("version::checked");
    // Conditional, against its being deleted, or given up on ([`abandon`]). A head's bytes
    // repeat only for the same change, which is then made.
    let (made, _) = change_leaving(bucket, child, start, |found| match found {
        None => Err(Error::NoCell),
        Some(found) if found.deleted() => Err(Error::Deleted),
        Some(found @ Head { made: Made::Forking { origin, .. }, .. }) => {
            Ok(Step::Write(Head { made: Made::Clone(origin), ..found }, true))
        }
        Some(_) => Ok(Step::Stop(false)),
    })
    .await
    .map_err(given_up)?;
    if made {
        failpoint!("version::made");
    }
    Ok(())
}

/// cladefs: `error`, a fork's step's, as the fork takes it: a head failed as it was made, by its
/// deletion or by the collector, its maker slow (TASK-691), the fork given up on, as when its head
/// says so ([`Error::Deleted`]).
fn given_up(error: Error) -> Error {
    match error {
        Error::Unmade(_) => Error::Deleted,
        error => error,
    }
}

/// Gives up on `child`, a fork being made of a version that was deleted ([`Made::Forking`]), so
/// that nothing keeps what it names, unless the fork was made first: returns its head as it then
/// stands. Its fork, which fails then, wrote nothing: a cell that holds nothing and that nothing
/// relies on, what it names perhaps gone, which recovering it would stop the collector on.
pub(crate) async fn abandon(bucket: &impl Bucket, child: CellId) -> Result<Head, Error> {
    change(bucket, child, Start::Read, |found| match found {
        None => Err(Error::NoCell),
        Some(found) if found.deleted() || found.forking().is_none() => Ok(Step::Stop(found)),
        Some(found) => {
            let holder = found.holder;
            let abandoned = ended(found, holder);
            Ok(Step::Write(abandoned.clone(), abandoned))
        }
    })
    .await
}

/// `head`, ended: its final epoch, which no writer takes, as `holder` ended it. What it names, it
/// keeps: the head the collector recovers the cell from, which nothing swaps after, so that every
/// deletion of the cell finds the same forks (TASK-183); but its versions, which go with the cell
/// (doc-18, TASK-476).
fn ended(head: Head, holder: u64) -> Head {
    Head { epoch: Head::FINAL, holder, token: u64::MAX, versions: Vec::new(), ..head }
}

/// Deletes `cell`, as `holder`, of the placement whose fencing token is `token`: one swap of its
/// head to its final epoch, which no writer takes again, the one it has stopping at its next swap,
/// and the collector reclaims what no other cell inherited from it. Deleting it again does
/// nothing, though the collector reclaimed that since. A cell with no head, a fork's child before
/// its head or no cell, gets one deleted from the first ([`Made::Own`]), its owner the deletion's,
/// which no other's is. Fails for a cell a later placement took, whose token is higher
/// ([`Error::Stale`]), leaving it as it was: a placement cut off deletes no cell from under the one
/// after it (TASK-45.1).
pub async fn delete(
    bucket: &impl Bucket,
    cell: CellId,
    holder: u64,
    token: u64,
) -> Result<(), Error> {
    delete_if(bucket, cell, (holder, token), holder, |_| true).await.map(drop)
}

/// Deletes `cell` as [`delete`] does, if `ours` says its head is one to delete, one with none
/// getting a head of `owner`'s: returns whether it is deleted, now or before, whoever's; `false`
/// for a head left as it is. cladefs: a head being made, or left unmade, `ours`, it fails, gone
/// for good, its maker's making failing with it (TASK-691, specs/head-slots); one failed before is
/// gone too; one its maker made whole meanwhile it deletes; another's, or one being removed, it
/// leaves.
async fn delete_if(
    bucket: &impl Bucket,
    cell: CellId,
    (holder, token): (u64, u64),
    owner: u64,
    ours: impl Fn(&Head) -> bool,
) -> Result<bool, Error> {
    let none = ended(Head::of(Made::Own, owner), holder);
    loop {
        let deleted = change(bucket, cell, Start::Read, |found| match found {
            None => Ok(Step::Write(none.clone(), true)),
            Some(found) if found.deleted() => Ok(Step::Stop(true)),
            Some(found) if !ours(&found) => Ok(Step::Stop(false)),
            Some(found) if rules().delete_tokens && found.token > token => Err(Error::Stale),
            Some(found) => Ok(Step::Write(ended(found, holder), true)),
        })
        .await;
        match deleted {
            Err(Error::Unmade(_)) => match head::fail(bucket, cell, &ours).await? {
                head::Failing::Failed => return Ok(true),
                head::Failing::Whole => {}
                head::Failing::Not => return Ok(false),
            },
            deleted => {
                let deleted = deleted?;
                failpoint!("lifecycle::head_ended");
                return Ok(deleted);
            }
        }
    }
}

/// Deletes `child`, the child of a fork of a version, as [`delete`] does, but only if its cell is
/// the one the fork made: its head names the fork's `owner`, or it has none yet, which gets one
/// deleted from the first, for good, so that a create of its head landing late, a dead host's,
/// finds it so. A cell of another's, whose ID the child was given, is left as it is (TASK-79.82);
/// so is a cell deleted already, whoever's. Returns whether the fork's cell is gone, deleted now
/// or before: `false` for another's. cladefs: a head its making left unmade, failed (TASK-691).
pub async fn delete_fork(
    bucket: &impl Bucket,
    child: CellId,
    owner: u64,
    (holder, token): (u64, u64),
) -> Result<bool, Error> {
    let ours = |found: &Head| found.is_child_of(owner);
    delete_if(bucket, child, (holder, token), owner, ours).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::test_cell;
    use crate::testing::{Direct, SimBucket};
    use futures::executor::block_on;

    /// `cell`'s head, if it has one.
    fn read(bucket: &impl Bucket, cell: CellId) -> Option<Head> {
        block_on(head::get(bucket, cell)).unwrap()
    }

    /// Takes `cell`'s head as `holder`, of token 0, giving it nothing.
    fn take(bucket: &impl Bucket, cell: CellId, holder: u64) -> Result<head::Held, Error> {
        let taking = head::Taking { holder, ..head::Taking::default() };
        block_on(async {
            head::take(bucket, cell, head::read(bucket, cell).await.unwrap(), &taking).await
        })
    }

    /// TASK-79.82: a fork's child is deleted only if its cell is the fork's: one whose head names
    /// the fork's owner, or one with none, which gets a head deleted from the first. One whose
    /// head names another owner is left as it is. One deleted is gone already.
    #[test]
    fn forks_delete_only_their_own_cells() {
        let bucket = Direct(SimBucket::new());
        let live = |cell| read(&bucket, cell).map(|head| !head.deleted());
        let unfork = |cell, owner| block_on(delete_fork(&bucket, cell, owner, (9, 0)));
        // A clone's, made.
        let origin = Origin { parent: test_cell(1), at: 7, epoch: 1 };
        let made = Head::of(Made::Clone(origin), 0x33);
        block_on(crate::testing::make_head(&bucket, test_cell(3), made)).unwrap();
        assert_eq!(unfork(test_cell(3), 0x33), Ok(true));
        assert_eq!(live(test_cell(3)), Some(false));
        assert_eq!(unfork(test_cell(3), 0x33), Ok(true), "gone already");
        // One with no head yet.
        assert_eq!(unfork(test_cell(4), 0x44), Ok(true));
        assert_eq!(live(test_cell(4)), Some(false));
        // Another's.
        block_on(create(&bucket, test_cell(5), 0x55)).unwrap();
        assert_eq!(unfork(test_cell(5), 0x50), Ok(false));
        assert_eq!(live(test_cell(5)), Some(true));
        let taken = take(&bucket, test_cell(5), 15);
        assert_eq!(taken.map(|held| held.head.epoch), Ok(1));
    }

    /// TASK-476: a restore of a version is made while its cell's head names it there, into a cell
    /// of its tenant's or, shared, of any; refused once it is dropped, or its cell deleted, its
    /// child's head then deleted by its host. Into another cell than its cell's, it writes the
    /// cell's checkpoint at the version first, which the child recovers through by its key; into
    /// its own, none.
    #[test]
    fn restores_of_versions_need_them_named() {
        use crate::batch::{Commit, SegmentBuilder};
        use crate::layout::NewVersion;
        use crate::writer::Writer;
        let memory = std::sync::Arc::new(object_store::memory::InMemory::new());
        let bucket = std::sync::Arc::new(crate::object_bucket::ObjectBucket::new(memory, ""));
        let cell = test_cell(1);
        let writer = block_on(async {
            create(&*bucket, cell, 0).await.unwrap();
            Writer::open(bucket.clone(), cell, 10, 0).await.unwrap()
        });
        let cut = NewVersion { id: 0xa, keep_last: None };
        let batch = SegmentBuilder::from(Commit { tag: 1, ..Commit::default() });
        let at = writer
            .add_segment(batch.cut(cut, &mut writer.chain()).frame(writer.keys().encrypting()))
            .unwrap();
        block_on(writer.upload(at)).unwrap();
        block_on(writer.acknowledge()).unwrap();
        let origin = Origin { parent: cell, at, epoch: writer.epoch() };
        let celled = |tenant, id| CellId { tenant, id };
        let restore = |child: CellId, id| {
            let kms = crate::keys::testing::host_kms();
            block_on(fork_version(&*bucket, child, id, origin, child.id, Parents::Kms(&kms, None)))
        };
        let record = |cell| read(&*bucket, cell);
        let checkpoint = layout::checkpoint(cell, at, writer.epoch());
        let checkpointed = || block_on(bucket::get(&*bucket, &checkpoint)).unwrap().is_some();

        assert_eq!(restore(celled(1, 3), 0xb), Err(Error::Deleted), "no such version");
        assert_eq!(restore(celled(1, 4), 0xa), Ok(()));
        assert!(checkpointed(), "a restore into another cell writes the checkpoint at the version");
        let recovered = block_on(recovery::recover(&*bucket, celled(1, 4), Some(origin)));
        assert_eq!(recovered.map(drop), Ok(()), "recovered through the checkpoint, by its key");

        assert_eq!(restore(celled(2, 5), 0xa), Err(Error::NotShared(cell)));
        block_on(writer.share_version(0xa, true)).unwrap();
        assert_eq!(restore(celled(2, 6), 0xa), Ok(()));
        block_on(writer.drop_versions(&std::collections::BTreeSet::from([0xa]))).unwrap();
        assert_eq!(restore(celled(1, 7), 0xa), Err(Error::Deleted), "dropped");
        assert_eq!(
            record(celled(1, 7)).map(|head| head.made),
            Some(Made::Forking { origin, version: 0xa })
        );
        let made = record(celled(2, 6)).map(|head| (head.made, head.epoch, head.owner));
        assert_eq!(made, Some((Made::Clone(origin), 0, 6)), "made, it stays");
    }

    /// TASK-160.14: whatever made a cell's head, whatever else makes one for its ID, of
    /// another owner, finds it another's, and changes nothing of it: a create, a restore of a
    /// version, and deleting a fork.
    #[test]
    fn makers_of_one_id_take_nothing_of_each_others() {
        use crate::testing::{VERSION, fork_version, versioned};
        // A bucket whose cell 1 names a version, which a restore forks.
        let bucket = || {
            let bucket = std::sync::Arc::new(Direct(SimBucket::new()));
            let (_, origin) = block_on(versioned(&bucket, test_cell(1), Vec::new()));
            (bucket, origin)
        };
        let (origin, child) = (bucket().1, 2);
        let said = |result: &dyn std::fmt::Debug| format!("{result:?}");
        let unfork =
            |bucket: &Direct, owner| block_on(delete_fork(bucket, test_cell(child), owner, (9, 0)));
        // Each maker, of the owner it is given, and what it gives, in words, finding another's
        // head, live: a create leaves it as it is, a fork finds its ID taken, deleting a fork
        // finds it another's. The first two make live heads.
        type Maker<'a> = (&'a str, &'a dyn Fn(&Direct, u64) -> String, &'a str);
        let taken = "Err(IdTaken(CellId { tenant: 1, id: 2 }))";
        let makers: [Maker; 3] = [
            (
                "create",
                &|bucket, owner| said(&block_on(create(bucket, test_cell(child), owner)).map(drop)),
                "Ok(())",
            ),
            (
                "fork_version",
                &|bucket, owner| {
                    said(&block_on(fork_version(bucket, test_cell(child), VERSION, origin, owner)))
                },
                taken,
            ),
            ("delete_fork", &|bucket, owner| said(&unfork(bucket, owner)), "Ok(false)"),
        ];
        for (first, make, _) in &makers[..2] {
            for (second, again, another) in &makers {
                let (bucket, _) = bucket();
                assert_eq!(make(&bucket, 0x11), "Ok(())", "{first}");
                let made = |bucket: &Direct| read(bucket, test_cell(child));
                let before = made(&bucket);
                assert_eq!(again(&bucket, 0x12), *another, "{second} after {first}");
                assert_eq!(made(&bucket), before, "{second} after {first}");
            }
        }
    }

    /// A cell deleted is taken no more: one with a head, ended, keeping what it was made as; one
    /// with none, given a head deleted from the first, of the deletion's owner, its holder's, which
    /// no other's is. A deletion of a cell a later placement took, of a higher token, is refused.
    #[test]
    fn deleted_cells_take_no_head() {
        let bucket = Direct(SimBucket::new());
        block_on(create(&bucket, test_cell(3), 0x33)).unwrap();
        assert_eq!(take(&bucket, test_cell(3), 1).map(|held| held.head.epoch), Ok(1));
        block_on(delete(&bucket, test_cell(3), 2, 0)).unwrap();
        let ended = read(&bucket, test_cell(3)).unwrap();
        assert_eq!((ended.epoch, ended.made, ended.owner), (Head::FINAL, Made::Root, 0x33));
        assert!(matches!(take(&bucket, test_cell(3), 3), Err(Error::Deleted)));
        block_on(delete(&bucket, test_cell(4), 2, 0)).unwrap();
        // cladefs: made whole, deleted from the first: its newest the maker's last copy (TASK-691).
        let swaps = crate::identity::DEFAULT_HEAD_SLOTS - 1;
        let deleted = Head {
            epoch: Head::FINAL,
            holder: 2,
            token: u64::MAX,
            swaps,
            ..Head::of(Made::Own, 2)
        };
        assert_eq!(read(&bucket, test_cell(4)), Some(deleted));
        let taking = head::Taking { holder: 5, token: 7, ..head::Taking::default() };
        block_on(create(&bucket, test_cell(5), 0x55)).unwrap();
        block_on(async {
            head::take(
                &bucket,
                test_cell(5),
                head::read(&bucket, test_cell(5)).await.unwrap(),
                &taking,
            )
            .await
        })
        .unwrap();
        assert_eq!(block_on(delete(&bucket, test_cell(5), 6, 6)), Err(Error::Stale));
        assert_eq!(block_on(delete(&bucket, test_cell(5), 6, 7)), Ok(()));
    }
}
