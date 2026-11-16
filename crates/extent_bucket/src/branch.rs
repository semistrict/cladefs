// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Branches' records: creating a branch, forking one, taking one over, deleting one.
//!
//! A fork of a branch with a writer is the writer's commit ([`record_fork`] gives its child its
//! record once the commit is acknowledged). A fork of a point is its child's record alone, which
//! writes nothing of the point's, so that any number of hosts fork one at once ([`fork_point`]).
//!
//! cladefs: every change of a record, a cell's object or a collector lease is a [`transition`],
//! one compare-and-swap loop, of what it found to what it writes (TASK-160.14).

use crate::bucket::{self, Bucket, Created, Object, Updated};
use crate::head;
use crate::layout::{self, Branch, Cell, Child, Head, Kind as Key, Origin};
use crate::record::{Kind, Record};
use crate::recovery::{self, recover_from};
use crate::{Error, rules};
use futures::StreamExt as _;
use serde::Serialize;
use std::collections::BTreeMap;

/// What a [`transition`] makes of the object it found.
pub(crate) enum Step<T, R> {
    /// Writes `T` in place of what it found, if that is still there, and gives `R`.
    Write(T, R),
    /// Writes nothing, and gives `R`.
    Stop(R),
}

/// What a [`transition`] starts from: and a holding of a cell ([`crate::cell::hold_from`]).
pub enum Start {
    /// What a read finds.
    Read,
    /// Nothing, as a create expects: one that finds something there reads it from its answer.
    Create,
    /// What a read before found: a write after it finds the object changed, if another's write
    /// came between, and reads it again, as it does after its own read.
    Found(Option<Object>),
}

/// Changes the object at `key`, which `decode` reads, as `step` says of what is there, if anything,
/// starting from `start`: by a create where nothing is, and a conditional update otherwise, given
/// to `step` again, from what is there then, whenever another's write came first, until it writes
/// nothing or what it writes lands. Every change of a branch's record, a cell's object or a
/// collector lease is one, its rules in its `step`. A write of it that landed though its answer
/// was lost is taken for its own ([`bucket::create`], [`bucket::update`]), so that what `step`
/// writes must differ from every other writer's: a record names its owner, the others their
/// holders.
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
        Start::Read => bucket::get(bucket, key).await,
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
            None => match bucket::create(bucket, key, bytes.clone()).await {
                Created::Created(version) => return Ok((result, Some(written(version)))),
                Created::Exists(object) => Some(object),
            },
            Some(object) => {
                match bucket::update(bucket, key, bytes.clone(), &object.meta.version).await {
                    Updated::Updated(version) => return Ok((result, Some(written(version)))),
                    Updated::Changed => bucket::get(bucket, key).await,
                }
            }
        };
    }
}

/// Changes `branch`'s record as `step` says ([`transition`]).
async fn change<R>(
    bucket: &impl Bucket,
    branch: Branch,
    start: Start,
    step: impl FnMut(Option<Record>) -> Result<Step<Record, R>, Error>,
) -> Result<R, Error> {
    let decode = |bytes: &[u8]| Record::decode(branch, bytes);
    transition(bucket, &layout::record(branch), start, decode, step).await
}

/// Changes `branch`'s record as [`change`] does, with the record object it left
/// ([`transition_leaving`]).
async fn change_leaving<R>(
    bucket: &impl Bucket,
    branch: Branch,
    start: Start,
    step: impl FnMut(Option<Record>) -> Result<Step<Record, R>, Error>,
) -> Result<(R, Option<Object>), Error> {
    let decode = |bytes: &[u8]| Record::decode(branch, bytes);
    transition_leaving(bucket, &layout::record(branch), start, decode, step).await
}

/// The owner every record a writer makes names, its maker having chosen `owner`
/// ([`Record::owner`]): unless the rule that they do is off, in a test that shows it needed.
fn owner(owner: u64) -> u64 {
    if rules().records_owned { owner } else { 0 }
}

/// Creates `branch`, of no parent, of `owner`, which its maker chose at random: a create of
/// another's finds its record there.
pub async fn create(bucket: &impl Bucket, branch: Branch, owner: u64) -> Result<(), Error> {
    let record = Kind::Root.of(self::owner(owner));
    change(bucket, branch, Start::Create, |found| match found {
        None => Ok(Step::Write(record.clone(), ())),
        Some(_) => Err(Error::Corrupt(format!("branch {branch} exists"))),
    })
    .await
}

/// Creates `child`, a fork's child taking a branch of its own, of no parent, its fork not made in
/// object storage, of the fork's `owner`. Its record says it is a fork's, of that owner, so that
/// it takes no record of another branch's that took its ID for its own, another fork's to that ID
/// among them, which a create of the same bytes would ([`Error::IdTaken`], TASK-79.82).
pub async fn create_forked(bucket: &impl Bucket, child: Branch, owner: u64) -> Result<(), Error> {
    let record = Kind::Own.of(self::owner(owner));
    change(bucket, child, Start::Create, |found| match found {
        None => Ok(Step::Write(record.clone(), ())),
        Some(_) => Err(Error::IdTaken(child)),
    })
    .await
}

/// cladefs: gives `branch`'s record the DEKs `deks` says it should have, its own and those it
/// inherits, given what it finds there, by a conditional update ([`transition`]); none, it is left
/// as it is (TASK-411.3, TASK-411.4, [`crate::keys`]). The writer's of the branch alone, holding
/// its cell, or its fork's host, before any store of it opens. Returns the record as it left it:
/// [`Error::NoBranch`] if it has none, [`Error::Deleted`] if it was deleted.
pub async fn give_deks(
    bucket: &impl Bucket,
    branch: Branch,
    deks: impl FnMut(&Record) -> Option<(Vec<crate::keys::Dek>, Vec<crate::keys::InheritedDek>)>,
) -> Result<Record, Error> {
    give_deks_from(bucket, branch, Start::Read, deks).await
}

/// [`give_deks`], from `found`, `branch`'s record as a read before found it, raw: so that a fork's
/// host reads it as it makes the child's cell, a round trip less (TASK-411.4).
pub(crate) async fn give_deks_found(
    bucket: &impl Bucket,
    branch: Branch,
    found: Option<Object>,
    deks: impl FnMut(&Record) -> Option<(Vec<crate::keys::Dek>, Vec<crate::keys::InheritedDek>)>,
) -> Result<Record, Error> {
    give_deks_from(bucket, branch, Start::Found(found), deks).await
}

async fn give_deks_from(
    bucket: &impl Bucket,
    branch: Branch,
    start: Start,
    mut deks: impl FnMut(&Record) -> Option<(Vec<crate::keys::Dek>, Vec<crate::keys::InheritedDek>)>,
) -> Result<Record, Error> {
    change(bucket, branch, start, |found| match found {
        None => Err(Error::NoBranch),
        Some(found) if found.deleted => Err(Error::Deleted),
        Some(found) => match deks(&found) {
            Some((deks, inherited)) => {
                let given = Record { deks, inherited, ..found };
                Ok(Step::Write(given.clone(), given))
            }
            None => Ok(Step::Stop(found)),
        },
    })
    .await
}

/// `branch`'s record, if it has one: [`Error::Format`] if it is of another format than this one,
/// whose objects this reads none of.
pub async fn record(bucket: &impl Bucket, branch: Branch) -> Result<Option<Record>, Error> {
    match bucket::get(bucket, &layout::record(branch)).await {
        Some(object) => Ok(Some(Record::decode(branch, &object.bytes)?)),
        None => Ok(None),
    }
}

/// The branches of `cell` whose records say they are live, with their records: an operator's look,
/// to compare with what hosts hold. A fork whose child has no record yet isn't among them. cladefs: a
/// cell's, listing its records alone (`cladefs-gc --branches`, TASK-395.4).
pub async fn live(bucket: &impl Bucket, cell: Cell) -> Result<BTreeMap<Branch, Record>, Error> {
    let mut live = BTreeMap::new();
    for meta in bucket::list(bucket, &layout::records(cell)).await {
        if let Key::Record { branch } = layout::kind(&meta.key)
            && let Some(record) = record(bucket, branch).await?
            && !record.deleted
        {
            live.insert(branch, record);
        }
    }
    Ok(live)
}

/// cladefs: writes the reference of the fork of `origin` to `child`, whose owner is `owner`, under
/// the parent's cell ([`layout::reference`], TASK-395.3): create-only, its bytes the fork's alone
/// ([`layout::Reference`]), so that every writer of it, and a write landed though its answer was
/// lost, writes the same. One there already stands, whatever its bytes: a pass keeps by its key.
async fn refer(bucket: &impl Bucket, origin: Origin, child: Branch, owner: u64) {
    let key = layout::reference(origin.parent, origin.at, child);
    bucket::create(bucket, &key, layout::encode(&layout::Reference::of(owner))).await;
}

/// cladefs: writes the reference of the writable fork of `origin` to `child`, if the child is of
/// another cell than its parent's ([`record_fork`], TASK-395.3).
pub(crate) async fn refer_fork(bucket: &impl Bucket, child: Child, origin: Origin) {
    if rules().references && origin.parent.cell != child.branch.cell {
        refer(bucket, origin, child.branch, owner(child.owner)).await;
    }
}

/// cladefs: retires the references at `keys` ([`layout::reference`]), garbage for good, under any
/// cells: marks each ([`layout::retired`]), then deletes them, many at once (TASK-395.13,
/// TASK-395.14). One a writer whose read of its child's record came before writes again after, the
/// pass over its parent's cell deletes, by its mark. Without the rule, as a test shows it needed,
/// deleted alone.
pub(crate) async fn retire(bucket: &impl Bucket, keys: &[&str]) {
    if rules().retirements_marked {
        let marks: Vec<String> = keys.iter().filter_map(|key| layout::retired_of(key)).collect();
        // The creates' futures made first, as `bucket::delete_all`'s batches are: a stream mapping
        // them in holds the closure in its type, which a future awaiting this one is then not seen
        // to be `Send` with.
        let marking: Vec<_> = marks.iter().map(|mark| mark_retired(bucket, mark)).collect();
        futures::stream::iter(marking).buffer_unordered(MARKS_AT_ONCE).collect::<()>().await;
    }
    bucket::delete_all(bucket, keys).await;
}

/// How many references' marks [`retire`] writes at once.
const MARKS_AT_ONCE: usize = 16;

/// Writes the mark `key` ([`layout::retired`]): there already, it stands, its bytes everyone's.
async fn mark_retired(bucket: &impl Bucket, key: &str) {
    bucket::create(bucket, key, layout::encode(&layout::Retired::NOW)).await;
}

/// [`retire`]s the reference at `key` alone, a request a step, with no stream of batches, so that a
/// writer's future, which the uploader's runtime needs `Send`, may await it (TASK-395.10).
async fn retire_one(bucket: &impl Bucket, key: &str) {
    if rules().retirements_marked
        && let Some(mark) = layout::retired_of(key)
    {
        mark_retired(bucket, &mark).await;
    }
    bucket::delete(bucket, key).await;
}

/// cladefs: retires the reference of the fork of `origin` to `child`, whose record, `found`, names
/// another origin than the fork's, or none (TASK-395.10): the fork made no branch, and no record
/// will name it, records being kept and their origins never changed, so that no pass would retire
/// it ([`crate::collector`] retires a reference by the origin its child's deleted record names).
/// Whoever finds it so takes it back: every writer of the child's record, and of a clone's; one
/// dying first leaves it to the next, the parent's next writer or its deletion recording the fork
/// again. One whose record names the fork's origin, another making's of the same fork, a clone of
/// a point there, stays: its retirement is that record's. Marked ([`retire`]): a writer that read
/// no record before the take-back writes it after, and may die before its own (TASK-395.13).
async fn take_back(bucket: &impl Bucket, origin: Origin, child: Branch, found: &Record) {
    if rules().references && rules().references_taken_back && found.origin() != Some(origin) {
        retire_one(bucket, &layout::reference(origin.parent, origin.at, child)).await;
    }
}

/// Gives `child`, a fork's, its record, forked from `origin`, of its owner, a point if it is one,
/// unless it has one: the same, whoever gives it, as the fork's footer names it. A fork's child
/// gets it once the fork is acknowledged, from the parent's writer, or the next one, or the
/// parent's deletion. One of another owner is another branch's ([`Error::IdTaken`]).
///
/// cladefs: a child of another cell than its parent's gets its reference first, under the parent's
/// cell, of the fork's owner (TASK-395.3): the pass over the parent's cell lists nothing of the
/// child's, and keeps the parent's state at the fork point by the reference, which a pass reading
/// the parent deleted lists after its deletion marked it, after this. A child of the parent's own
/// cell needs none: that cell's pass reads its record. Its record is read first, so that a child
/// deleted, or another's, whose ID the fork's guest named, gets none: nothing inherits through
/// either. A record found, or one that lands between the read and the create, naming another
/// origin than the fork's, or none (a fork deleted before its host learned its origin,
/// `Kind::Own`), has the reference taken back ([`take_back`], TASK-395.10), by this writer of the
/// record or, it dying first, the next, which reads it so. The read costs a request a fork across
/// cells, each of its writers. A child recorded already, ours, live, gets neither the checkpoint
/// nor its reference again: whoever recorded it wrote both first (TASK-395.14), and the reference
/// written again might land after the child's deletion and its cell's pass retiring it, which that
/// cell's passes, skipping it, dead and unchanged, would never retire again. A writer that read no
/// record still may, the fork recorded since by another, and the child deleted; or after another
/// took it back, the child's ID taken (TASK-395.13): every retirement is marked, and the pass over
/// the parent's cell, whose listing the write changes, deletes it ([`retire`]).
///
/// cladefs: before both, for a child of another cell, the parent's checkpoint at the fork point
/// ([`recovery::checkpoint_at`]): what the child's recovery reads of its parent by its key alone,
/// so that a pass over the child's cell lists nothing of the parent's (TASK-395.4). A point's
/// clones, of any cell, read the same, which its own recording wrote: a point in its origin's own
/// cell, which no host makes, has none, and its clones into other cells recover not. The parent's
/// state there is gone only once the child was deleted, since this read found it with no record
/// (TASK-395.11): nothing else lets a pass go of it, the parent's state naming the fork until a
/// checkpoint drops it, which only a child deleted is, and the parent's deletion recording the
/// child before it marks the parent deleted. So the record is read again, and goes as any found;
/// none there would break that, and fails with [`Error::Lost`].
///
/// Without the rule that it comes first, as a test shows it needed, the record's writers write it
/// after: the writer once it recorded the fork, the deletion once it marked the parent deleted, as
/// specs/bucket's NoReferencesFirst has it.
///
/// cladefs: the parent's checkpoint at the fork point authenticated under the DEK `keys` give, the
/// parent's: its writer's ring, or its deletion's ([`crate::keys::Parents`], TASK-411.4).
pub async fn record_fork(
    bucket: &impl Bucket,
    child: Child,
    origin: Origin,
    keys: crate::keys::Parents<'_>,
) -> Result<(), Error> {
    failpoint!("branch::record_fork");
    let kind = if child.point { Kind::Point(origin) } else { Kind::Fork(origin) };
    let record = kind.of(owner(child.owner));
    let across = origin.parent.cell != child.branch.cell;
    let mut start = Start::Create;
    if across {
        let mut found = bucket::get(bucket, &layout::record(child.branch)).await;
        if found.is_none() {
            let checkpointed =
                recovery::checkpoint_at(bucket, origin.parent, origin.at, keys).await;
            // Read again, as the checkpoint may take long: a record made meanwhile, the child
            // deleted or its ID taken, is taken back from now rather than after the reference
            // (TASK-395.10; a writer dying between the reference and its create still leaves
            // one written after another's take-back: TASK-395.13). The state at the fork point
            // gone, the child was deleted since the read, its fork dropped and its fork point let
            // go of (TASK-395.11): its record says so.
            found = bucket::get(bucket, &layout::record(child.branch)).await;
            match checkpointed {
                Ok(()) => {}
                Err(Error::Lost(lost)) if found.is_none() => return Err(Error::Lost(lost)),
                Err(Error::Lost(_)) => {}
                Err(error) => return Err(error),
            }
        }
        let read =
            found.as_ref().map(|found| Record::decode(child.branch, &found.bytes)).transpose()?;
        match read {
            Some(found) if found.deleted || !found.is_child_of(record.owner) => {
                take_back(bucket, origin, child.branch, &found).await;
                return match found.deleted {
                    true => Ok(()),
                    false => Err(Error::IdTaken(child.branch)),
                };
            }
            // Recorded: whoever did wrote the checkpoint first, and recovering the parent there
            // again may find its segments gone, a checkpoint since covering them.
            Some(_) | None => {}
        }
        // None read, the reference is written; one read, ours, live, its recorder wrote it.
        if rules().references_first && found.is_none() {
            refer_fork(bucket, child, origin).await;
        }
        failpoint!("branch::referred");
        start = Start::Found(found);
    }
    // A record there: the fork's, made since the read, or deleted, or another's.
    let found = change(bucket, child.branch, start, |found| match found {
        None => Ok(Step::Write(record.clone(), None)),
        Some(found) => Ok(Step::Stop(Some(found))),
    })
    .await?;
    let Some(found) = found else {
        failpoint!("branch::recorded_fork");
        return Ok(());
    };
    if found.is_child_of(record.owner) && !found.deleted {
        return Ok(());
    }
    if across {
        take_back(bucket, origin, child.branch, &found).await;
    }
    match found.deleted {
        // Deleted before it got its record, perhaps.
        true => Ok(()),
        false => Err(Error::IdTaken(child.branch)),
    }
}

/// cladefs: the fork's reference, under the cell of `origin`'s branch, whatever `child`'s cell, that
/// branch's own too (TASK-395.3): the pass over that cell lists nothing of `child`'s, and keeps
/// `origin`'s state while the reference stands. Written before the point's record is read, so that
/// a fork that found the point live wrote it before the point's deletion, which a pass reading the
/// point deleted, or finding the point's own reference retired, read after; and listing the
/// references after it read the states, finds it. One in the origin's own cell too, which no host
/// makes: that cell's pass reads its records before the states and its references after, and a
/// clone made between them, its point in another cell deleted and the point's reference retired
/// before the references were listed, would be seen by neither (TLC found it, specs/bucket's
/// BucketOverlap).
///
/// And after its record (TASK-395.10): a reference always has a record naming its origin, by which
/// its child's cell's pass retires it, once the fork failed and its host deleted the child, or the
/// collector gave it up as its point went, or its host died, whose directory, left unmade, names
/// no child. A fork refused by a record there writes none. Written first, a host dying before the
/// record left a reference no record names, which nothing retired: specs/bucket's
/// NoForksRecordFirst. Forked again, the record its own, it writes it again, which stands.
///
/// cladefs: forked again, its record being made, it resumes the fork as another would
/// ([`resume_point_fork`]): the record said resumed first, with a condition, as the making that
/// began it may still write the reference after the clone was made (TASK-455's review).
///
/// cladefs: four round trips, one after another (TASK-449): the record's create; the reference's;
/// the point's record read; the record's update, conditional on the version its create left, not
/// read back. A record there already is what the create finds, as the read a host made of the
/// child's record first found (`Uploader::child_free`, which a fork of a point makes no more): ours
/// being made, resumed; deleted, or another's, the fork refused.
pub async fn fork_point(
    bucket: &impl Bucket,
    child: Branch,
    point: Branch,
    origin: Origin,
    owner: u64,
) -> Result<(), Error> {
    let nothing = std::future::ready(());
    fork_point_with(bucket, child, point, origin, owner, nothing).await.map(drop)
}

/// cladefs: [`fork_point`], with `alongside` run at once with the reference's write, once the
/// child's record is made, if this makes the fork: its host's holding of the child's cell, which
/// the child's store would hold first as it opens (TASK-449). Returns, if this made the fork, the
/// child's record as it left it, a clone made; and what `alongside` gave, if it ran.
pub async fn fork_point_with<T>(
    bucket: &impl Bucket,
    child: Branch,
    point: Branch,
    origin: Origin,
    owner: u64,
    alongside: impl Future<Output = T>,
) -> Result<(Option<Record>, Option<T>), Error> {
    failpoint!("point::record");
    let owner = self::owner(owner);
    let refers = rules().references && rules().clones_refer_first;
    // Without the rule, as a test shows it needed: the reference before the record.
    let refers_first = refers && !rules().clones_record_first;
    if refers_first {
        refer(bucket, origin, child, owner).await;
        failpoint!("point::referred");
    }
    let kind = match rules().forks_pending {
        true => Kind::Forking { origin, point },
        false => Kind::Clone(origin),
    };
    let record = kind.of(owner);
    // A record found that refused the fork.
    let mut refused = None;
    let making = change_leaving(bucket, child, Start::Create, |found| match found {
        None => Ok(Step::Write(record.clone(), true)),
        // Another's, deleted or not: its ID taken, as a read of the record before found.
        Some(found) if !found.is_child_of(record.owner) => {
            refused = Some(found);
            Err(Error::IdTaken(child))
        }
        // Ours, given up on, its point deleted.
        Some(found) if found.deleted => {
            refused = Some(found);
            Err(Error::Deleted)
        }
        // Ours, being made by another making of it, which it resumes.
        Some(found) if found.forking().is_some() && !found.resumed => {
            Ok(Step::Write(found.resumed(), true))
        }
        Some(found) => Ok(Step::Stop(found.forking().is_some())),
    })
    .await;
    if let Some(found) = refused.filter(|_| refers_first) {
        take_back(bucket, origin, child, &found).await;
    }
    let (making, left) = making?;
    let mut made = (None, None);
    if making {
        failpoint!("point::recorded");
        let referring = async {
            if refers && !refers_first {
                refer(bucket, origin, child, owner).await;
                failpoint!("point::referred");
            }
            Ok::<(), Error>(())
        };
        let (referred, beside) = futures::join!(referring, alongside);
        referred?;
        made =
            (finish_point_fork_from(bucket, child, point, Start::Found(left)).await?, Some(beside));
    }
    // Without the rule, as a test shows it needed: once made.
    if rules().references && !rules().clones_refer_first {
        refer(bucket, origin, child, owner).await;
    }
    Ok(made)
}

/// cladefs: resumes the fork of a point to `child` that another began ([`fork_point`]), its record
/// saying it is being made: a store opening `child`, its host dead or slow, makes it, so that it
/// takes no state of a fork not made (TASK-455's review). The record is said resumed first
/// ([`Record::resumed`]), with a condition, so that the clone made says so, whose reference's
/// retirement is then marked: the host that began it, slow, may write the reference after the
/// clone was made, deleted and its cell removed (rule `resumes_marked`). Then the reference, as
/// that host writes it, before the fork is made (rule `resumes_refer`): it may have died first.
/// Then the fork made ([`finish_point_fork`]). A fork made already, it writes nothing: its host,
/// or a resume, wrote the reference before. Fails as [`finish_point_fork`] does, and with
/// [`Error::Deleted`] for a fork given up on, [`Error::NoBranch`] for no record.
pub async fn resume_point_fork(bucket: &impl Bucket, child: Branch) -> Result<(), Error> {
    let forking = change(bucket, child, Start::Read, |found| match found {
        None => Err(Error::NoBranch),
        Some(found) if found.deleted => Err(Error::Deleted),
        Some(found) if found.forking().is_none() => Ok(Step::Stop(None)),
        Some(found) if found.resumed => Ok(Step::Stop(Some(found))),
        Some(found) => Ok(Step::Write(found.clone().resumed(), Some(found))),
    })
    .await?;
    let Some(Record { kind: Kind::Forking { origin, point }, owner, .. }) = forking else {
        return Ok(());
    };
    // Without the rule, as a test shows it needed: none written.
    if rules().references && rules().clones_refer_first && rules().resumes_refer {
        refer(bucket, origin, child, owner).await;
    }
    finish_point_fork(bucket, child, point).await
}

/// Makes the fork being made of `point` to `child` ([`fork_point`]), whose record is created, a
/// clone made ([`Kind::Clone`]), its reference written before by its one writer, its host: fails
/// with [`Error::Deleted`] if the point was deleted, or the fork was given up on. cladefs: and with
/// [`Error::NotShared`] if `child` is of another tenant's cell than the point, which its record,
/// as this read finds it, doesn't say is shared (TASK-395.7): unsharing it stops the forks whose
/// reads come after. Its host deletes `child` then, as for a point deleted.
pub async fn finish_point_fork(
    bucket: &impl Bucket,
    child: Branch,
    point: Branch,
) -> Result<(), Error> {
    finish_point_fork_from(bucket, child, point, Start::Read).await.map(drop)
}

/// [`finish_point_fork`], the child's record's update starting from `start`: the record as its
/// create left it, a read less (TASK-449), its update conditional on that version as on one a read
/// found, so that its deletion, its giving up, or a resume since makes it read the record anew.
/// Returns the record as it left it, if it made the fork.
async fn finish_point_fork_from(
    bucket: &impl Bucket,
    child: Branch,
    point: Branch,
    start: Start,
) -> Result<Option<Record>, Error> {
    let tenant = child.cell.tenant;
    if rules().forks_check_points || point.cell.tenant != tenant {
        let found = record(bucket, point).await?;
        if rules().forks_check_points && !found.as_ref().is_some_and(|record| !record.deleted) {
            return Err(Error::Deleted);
        }
        if !found.is_some_and(|record| record.may_fork_into(point, tenant)) {
            return Err(Error::NotShared(point));
        }
    }
    failpoint!("point::checked");
    // Conditional, against its being deleted, or given up on ([`abandon`]). A record's bytes
    // repeat only for the same change, which is then made.
    let made = change(bucket, child, start, |found| match found {
        None => Err(Error::NoBranch),
        Some(found) if found.deleted => Err(Error::Deleted),
        Some(found @ Record { kind: Kind::Forking { origin, .. }, .. }) => {
            let made = Record { kind: Kind::Clone(origin), ..found };
            Ok(Step::Write(made.clone(), Some(made)))
        }
        Some(_) => Ok(Step::Stop(None)),
    })
    .await?;
    if made.is_some() {
        failpoint!("point::made");
    }
    Ok(made)
}

/// cladefs: shares `point`, if `shared`, or no longer: other tenants' cells than its own may have
/// branches forked from it while it is shared, and no new ones once not ([`Record::shared`],
/// TASK-395.7); forks made stay. The trusted control API's alone: no guest asks for it. Fails with
/// [`Error::NotPoint`] for a branch that is no point, [`Error::Deleted`] for one deleted, and
/// [`Error::NoBranch`] for none.
pub async fn share(bucket: &impl Bucket, point: Branch, shared: bool) -> Result<(), Error> {
    change(bucket, point, Start::Read, |found| match found {
        None => Err(Error::NoBranch),
        Some(found) if found.deleted => Err(Error::Deleted),
        Some(found) if !found.is_point() => Err(Error::NotPoint(point)),
        Some(found) if found.shared == shared => Ok(Step::Stop(())),
        Some(found) => Ok(Step::Write(Record { shared, ..found }, ())),
    })
    .await
}

/// Whether no writer has `branch`: it has no head, or its deletion's.
async fn unwritten(bucket: &impl Bucket, branch: Branch) -> Result<bool, Error> {
    let epoch = head::epoch(bucket, branch).await?;
    Ok(epoch == 0 || epoch == Head::FINAL)
}

/// Whether recovering `branch`, whose record is `record`, failing, what it names gone
/// ([`Error::Lost`]), says nothing was lost: it is a failed fork's claim ([`Kind::Claim`]), which
/// no writer has. What it names may have gone before it landed; nothing ever read it.
pub(crate) async fn lost_is_ok(
    bucket: &impl Bucket,
    branch: Branch,
    record: &Record,
) -> Result<bool, Error> {
    Ok(matches!(record.kind, Kind::Claim(_)) && unwritten(bucket, branch).await?)
}

/// Deletes `branch`, whose recovery failed though nothing was lost ([`lost_is_ok`]): unless it
/// was given another record since. Nothing ever read it: no writer recovers what a record not
/// there names.
pub(crate) async fn abandon_claim(bucket: &impl Bucket, branch: Branch) -> Result<(), Error> {
    mark_deleted(bucket, branch).await
}

/// Gives up on `child`, a fork being made of a point that was deleted ([`Kind::Forking`]), so
/// that nothing keeps what it names, unless the fork was made first: returns its record as it then
/// stands. Its fork, which fails then, wrote nothing: a branch that holds nothing and that nothing
/// relies on, what it names perhaps gone, which recovering it would stop the collector on.
pub(crate) async fn abandon(bucket: &impl Bucket, child: Branch) -> Result<Record, Error> {
    change(bucket, child, Start::Read, |found| match found {
        None => Err(Error::NoBranch),
        Some(found) if found.deleted || found.forking().is_none() => Ok(Step::Stop(found)),
        Some(found) => {
            let abandoned = Record { deleted: true, ..found };
            Ok(Step::Write(abandoned.clone(), abandoned))
        }
    })
    .await
}

/// Deletes `branch`, as `holder`, of the placement whose fencing token is `token`: no writer takes
/// it again, the one it has stops at its next swap of the head, and the collector reclaims what no
/// other branch inherited from it. Deleting it again does nothing, though the collector reclaimed
/// that since. Fails for a branch a later placement took, whose token is higher ([`Error::Stale`]),
/// leaving it as it was: a placement cut off deletes no branch from under the one after it
/// (TASK-45.1).
///
/// cladefs: by a host whose KMS is `kms`: the branch's forks to other cells it records get its
/// checkpoint at their fork points authenticated under its DEK, its cell's KEK unwrapped by `kms`
/// ([`crate::keys::writing`], TASK-411.4), asked only then.
pub async fn delete(
    bucket: &impl Bucket,
    branch: Branch,
    holder: u64,
    token: u64,
    kms: &std::sync::Arc<crate::keys::CachingKms>,
) -> Result<(), Error> {
    let ended = end_head(bucket, branch, holder, token).await?;
    failpoint!("branch::head_ended");
    // A fork's child before its record, or no branch: either way none is, from now, unless its
    // record came meanwhile. Its owner the deletion's, which no other's is.
    let deleted = Record { deleted: true, ..Kind::Own.of(owner(holder)) };
    let found = change(bucket, branch, Start::Read, |found| match found {
        None => Ok(Step::Write(deleted.clone(), None)),
        Some(found) => Ok(Step::Stop(Some(found))),
    })
    .await?;
    // Deleted already: its children got their records before, and what recovering it read, no
    // live branch needs, the collector may have deleted since (TASK-79.78).
    let Some(record) = found.filter(|record| !record.deleted) else { return Ok(()) };
    // Its children get their records, so that the collector finds them without its state. A fork
    // of a point still being made has none, and may name what is gone; so may a claim, with
    // neither a writer nor a child, if it does. Recovered from the head it ended, which nothing
    // swaps after: every deletion of it finds the same forks (TASK-183).
    if record.forking().is_none() {
        let keyless = crate::keys::Ring::keyless();
        let forks = match recover_from(bucket, branch, record.origin(), Some(ended), &keyless).await
        {
            Ok(recovered) => recovered.state.forks,
            // Deleted meanwhile, by another deletion, which gave its children their records: what
            // recovering it needs, the collector may have reclaimed since (TASK-178).
            Err(Error::Lost(_))
                if self::record(bucket, branch).await?.is_none_or(|r| r.deleted) =>
            {
                return Ok(());
            }
            Err(Error::Lost(_)) if lost_is_ok(bucket, branch, &record).await? => Vec::new(),
            Err(error) => return Err(error),
        };
        // cladefs: under its keys, for its checkpoints at the fork points of its forks to other
        // cells, as they are written (TASK-411.4).
        let keys = crate::keys::Parents::Kms(kms, Some(&record));
        for fork in &forks {
            let origin = Origin { parent: branch, at: fork.at };
            let child = Child { branch: fork.child, owner: fork.owner, point: fork.point };
            match record_fork(bucket, child, origin, keys).await {
                Ok(()) | Err(Error::IdTaken(_)) => {}
                Err(error) => return Err(error),
            }
        }
        failpoint!("branch::children_recorded");
        mark_deleted(bucket, branch).await?;
        // Without the rule that they come first, as a test shows it needed: the references after.
        if !rules().references_first {
            for fork in &forks {
                let origin = Origin { parent: branch, at: fork.at };
                let child = Child { branch: fork.child, owner: fork.owner, point: fork.point };
                refer_fork(bucket, child, origin).await;
            }
        }
        return Ok(());
    }
    failpoint!("branch::children_recorded");
    mark_deleted(bucket, branch).await
}

/// Ends `branch`'s head, as [`delete`] does first: its final epoch, which no writer takes, as
/// `holder` of the placement whose fencing token is `token`, unless a later placement took it
/// ([`Error::Stale`]). Returns it, ended: what it names, kept.
async fn end_head(
    bucket: &impl Bucket,
    branch: Branch,
    holder: u64,
    token: u64,
) -> Result<Head, Error> {
    let read = head::read(bucket, branch).await;
    // cladefs: ended already, by this deletion or another, whatever placement's: deletions are
    // idempotent. Else the token is resolved by the branch's cell (TASK-364).
    if let Some(found) = read.head()?
        && found.epoch == Head::FINAL
    {
        return Ok(found);
    }
    let token = deleting_token(bucket, branch, token).await?;
    head::end(bucket, branch, read, holder, token).await
}

/// cladefs: the token a deletion by the placement of `token` ends `branch`'s head with, as its
/// writers take it with their cell's epoch (TASK-364): the cell's epoch now, which takes the head
/// from its holder's writer, and from none a later taking of the cell made; refused if a later
/// placement holds the cell ([`Error::Stale`]). The deletion takes nothing of the cell, its other
/// vdisks' writers going on. `token` itself where the cell has no object, or was deleted, as then
/// no writer took the branch's head by its cell; and the highest token there is, an operator's,
/// whichever placement took the branch last.
async fn deleting_token(bucket: &impl Bucket, branch: Branch, token: u64) -> Result<u64, Error> {
    if token == u64::MAX {
        return Ok(token);
    }
    Ok(match crate::cell::read(bucket, branch.cell).await? {
        Some(cell) if cell.deleted => token,
        Some(cell) if rules().delete_tokens && cell.token > token => return Err(Error::Stale),
        Some(cell) => cell.epoch,
        None => token,
    })
}

/// Marks `branch`'s record deleted, as [`delete`] does last. A record's bytes repeat only for the
/// same change, which is then made.
async fn mark_deleted(bucket: &impl Bucket, branch: Branch) -> Result<(), Error> {
    change(bucket, branch, Start::Read, |found| match found {
        None => Err(Error::NoBranch),
        Some(found) if found.deleted => Ok(Step::Stop(())),
        Some(found) => Ok(Step::Write(Record { deleted: true, ..found }, ())),
    })
    .await
}

/// Deletes `child`, the child of a fork, as [`delete`] does, but only if its branch is the one the
/// fork made: its record names the fork's `owner`, whoever wrote it, the fork's host or its
/// parent's writers, or it has no record yet, as a fork whose segment has not been acknowledged
/// has not. `forked` is the kind of record the fork gives its child, as its host knows: a branch of
/// its own ([`Kind::Own`]), its fork's segment never having gone to object storage; a fork at a
/// segment, which may have landed, and whose branch's writers then give the child a record; a
/// point, so, whatever record it gets saying so, whoever gives it; or a fork of a point, which
/// nothing but the fork itself gives its child a record. A branch of another's, whose ID the
/// fork's child was given, as a guest may give any, another fork's to the same ID among them, is
/// left as it is (TASK-79.82); so is a branch deleted already, whoever's. Returns whether the
/// fork's branch is gone, deleted now or before: `false` for another's.
///
/// A child with no record yet gets one first, before the deletion ends its head: created only if
/// no record is there, which another branch taking the ID meanwhile has, whose head the deletion
/// took otherwise (TASK-79.82). Its record taken, the ID is no other's, and a deletion that stops
/// goes on as the fork's; it had no record before, nor children with none, as [`delete`] deletes
/// such a branch. The record is the one its fork would give it ([`record_fork`]), live, so that a
/// deletion a later placement refuses ([`Error::Stale`]) changes nothing, only if its parent's
/// branch holds the fork's segment, as a live branch's record must say: a child naming a segment
/// that is not there recovers from nothing, which fails its deletion, should one stopping go on,
/// and every pass of the collector. It is a claim ([`Kind::Claim`]): that the parent's branch
/// holds a segment of that number is no proof it is the fork's, and nothing keeps what it names
/// until the record lands, a dead host's perhaps, long after. Otherwise it is deleted from the
/// first, for good: no other placement took a branch nothing of which landed; and a segment
/// landing after finds it so. So is a fork of a point with no record, which holds nothing, a
/// create of its record, a dead host's landing late, finding it so too; and a point with none: no
/// placement refuses a deletion of what has no head, and a stale copy finds no live record naming
/// a segment that may not be its fork's.
///
/// cladefs: by a host whose KMS is `kms`: a claim of another cell than its parent's gets the
/// parent's checkpoint at the fork point authenticated under the parent's DEK, as
/// [`record_fork`] writes it (TASK-411.4).
pub async fn delete_fork(
    bucket: &impl Bucket,
    child: Branch,
    forked: Kind,
    owner: u64,
    (holder, token): (u64, u64),
    kms: &std::sync::Arc<crate::keys::CachingKms>,
) -> Result<bool, Error> {
    let owner = self::owner(owner);
    // What deleting the child, whose record is `record`, gives.
    let delete_found = async |record: Record| match record {
        Record { deleted: true, .. } => Ok(true),
        record if !record.is_child_of(owner) => Ok(false),
        _ => delete(bucket, child, holder, token, kms).await.map(|()| true),
    };
    if let Some(record) = record(bucket, child).await? {
        return delete_found(record).await;
    }
    if forked.origin().is_none() && !rules().fork_claims_ids {
        return delete(bucket, child, holder, token, kms).await.map(|()| true);
    }
    // The record it gets, and whether live, a claim, as its parent's branch holds its segment.
    let (kind, live) = match forked {
        Kind::Fork(origin) | Kind::Claim(origin) if !rules().fork_claims_landed => {
            (Kind::Fork(origin), true)
        }
        Kind::Fork(origin) | Kind::Claim(origin) => {
            // cladefs: a claim of another cell than its parent's lives only with the parent's
            // checkpoint at the fork point, which is what its recovery reads of the parent there
            // (TASK-395.4), as [`record_fork`] writes it: what is going since the parent's branch
            // was found holding the segment, as a pass takes it, reads as not held, as there.
            let held = match recovery::holds_fork(bucket, origin, child, owner).await? {
                true if origin.parent.cell != child.cell => {
                    // cladefs: under the parent's keys (TASK-411.4).
                    let keys = crate::keys::Parents::Kms(kms, None);
                    match recovery::checkpoint_at(bucket, origin.parent, origin.at, keys).await {
                        Ok(()) => true,
                        Err(Error::Lost(_)) => false,
                        Err(error) => return Err(error),
                    }
                }
                held => held,
            };
            match held {
                true => (Kind::Claim(origin), true),
                false => (Kind::Fork(origin), false),
            }
        }
        // A clone made has its record: one of none, never made, stays deleted for good, as the
        // collector removes no fork's (TASK-455).
        Kind::Forking { origin, .. } | Kind::Clone(origin) => (Kind::Fork(origin), false),
        Kind::Point(origin) => (Kind::Point(origin), false),
        Kind::Root | Kind::Own => (Kind::Own, false),
    };
    let record = Record { deleted: !live, ..kind.of(owner) };
    let taken = change(bucket, child, Start::Create, |taken| match taken {
        None => Ok(Step::Write(record.clone(), None)),
        Some(taken) => Ok(Step::Stop(Some(taken))),
    })
    .await?;
    if let Some(taken) = taken {
        return delete_found(taken).await;
    }
    failpoint!("branch::claimed");
    end_head(bucket, child, holder, token).await?;
    failpoint!("branch::claim_head_ended");
    if live {
        mark_deleted(bucket, child).await?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::child;
    use crate::layout::test_branch;
    use crate::testing::{Direct, SimBucket};
    use futures::executor::block_on;

    /// TASK-79.82: a fork's child is deleted only if its branch is the fork's: one with no record,
    /// or whose record names the fork's owner. One whose record names another owner, a branch of no
    /// parent's, or another fork's, at another point of the fork's parent or whose segment never
    /// went, is left as it is. One deleted is gone already.
    #[test]
    fn forks_delete_only_their_own_branches() {
        use crate::batch::{Commit, SegmentBuilder};
        use crate::writer::Writer;
        let memory = std::sync::Arc::new(object_store::memory::InMemory::new());
        let bucket = std::sync::Arc::new(crate::object_bucket::ObjectBucket::new(memory, ""));
        let live = |child| {
            let record = block_on(record(&*bucket, child)).unwrap();
            record.map(|record| !record.deleted)
        };
        let owned = |branch, owner| Child { branch, owner, point: false };
        // A fork of branch 1's to 3, made, its child given its record.
        let origin = block_on(async {
            create(&*bucket, test_branch(1), 0x11).await.unwrap();
            let parent = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
            let forking = SegmentBuilder::from(Commit { tag: 1, ..Commit::default() });
            let objects = forking
                .fork(owned(test_branch(3), 0x33), &mut parent.chain())
                .frame(parent.keys().encrypting());
            let at = parent.add_segment(objects).unwrap();
            parent.upload(at).await.unwrap();
            parent.acknowledge().await.unwrap();
            Origin { parent: test_branch(1), at }
        });
        assert_eq!(live(test_branch(3)), Some(true));
        assert_eq!(
            block_on(delete_fork(
                &*bucket,
                test_branch(3),
                Kind::Fork(origin),
                0x33,
                (9, 0),
                &crate::keys::testing::host_kms()
            )),
            Ok(true)
        );
        assert_eq!(live(test_branch(3)), Some(false));
        assert_eq!(
            block_on(delete_fork(
                &*bucket,
                test_branch(3),
                Kind::Own,
                0x33,
                (9, 0),
                &crate::keys::testing::host_kms()
            )),
            Ok(true)
        );
        // Forks whose children have no record yet, whose segments went or not.
        assert_eq!(
            block_on(delete_fork(
                &*bucket,
                test_branch(2),
                Kind::Fork(origin),
                0x22,
                (9, 0),
                &crate::keys::testing::host_kms()
            )),
            Ok(true)
        );
        assert_eq!(
            block_on(delete_fork(
                &*bucket,
                test_branch(4),
                Kind::Own,
                0x44,
                (9, 0),
                &crate::keys::testing::host_kms()
            )),
            Ok(true)
        );
        assert_eq!((live(test_branch(2)), live(test_branch(4))), (Some(false), Some(false)));

        block_on(create(&*bucket, test_branch(5), 0x55)).unwrap();
        let other = Origin { at: origin.at + 1, ..origin };
        block_on(record_fork(
            &*bucket,
            owned(test_branch(6), 0x66),
            other,
            crate::keys::Parents::Kms(&crate::keys::testing::host_kms(), None),
        ))
        .unwrap();
        block_on(record_fork(
            &*bucket,
            owned(test_branch(7), 0x77),
            origin,
            crate::keys::Parents::Kms(&crate::keys::testing::host_kms(), None),
        ))
        .unwrap();
        for (child, origin, owner) in
            [(5, Kind::Fork(origin), 0x50), (6, Kind::Fork(origin), 0x60), (7, Kind::Own, 0x70)]
        {
            let deleted = block_on(delete_fork(
                &*bucket,
                test_branch(child),
                origin,
                owner,
                (9, 0),
                &crate::keys::testing::host_kms(),
            ));
            assert_eq!(deleted, Ok(false), "{child}");
            assert_eq!(live(test_branch(child)), Some(true), "{child}");
            let taken = block_on(head::take(&*bucket, test_branch(child), 10 + child, 0));
            assert_eq!(taken.map(|held| held.head.epoch), Ok(1), "{child}");
        }
    }

    /// A fork's child gets one record: given again by whoever gives it, of the same owner, it
    /// stands; of another, the child is another branch's.
    #[test]
    fn a_child_gets_one_record() {
        let bucket = Direct(SimBucket::new());
        let origin = Origin { parent: test_branch(1), at: 7 };
        let forked = Child { branch: test_branch(2), owner: 0x22, point: false };
        block_on(record_fork(
            &bucket,
            forked,
            origin,
            crate::keys::Parents::Kms(&crate::keys::testing::host_kms(), None),
        ))
        .unwrap();
        block_on(record_fork(
            &bucket,
            forked,
            origin,
            crate::keys::Parents::Kms(&crate::keys::testing::host_kms(), None),
        ))
        .unwrap();
        let other = Child { owner: 0x23, ..forked };
        assert_eq!(
            block_on(record_fork(
                &bucket,
                other,
                origin,
                crate::keys::Parents::Kms(&crate::keys::testing::host_kms(), None)
            )),
            Err(Error::IdTaken(test_branch(2)))
        );
        let record = block_on(record(&bucket, test_branch(2))).unwrap();
        assert_eq!(record, Some(Kind::Fork(origin).of(0x22)));
    }

    /// TASK-395.7: a fork of a point to a branch of another tenant's cell is refused unless the
    /// point's record, as the fork reads it, says the point is shared; shared, the fork is made;
    /// unshared, new forks are refused again, and the fork made stands. Forks within the point's
    /// tenant need no sharing. Only a point is shared, and not once deleted.
    #[test]
    fn forks_of_points_into_other_tenants_need_them_shared() {
        let bucket = Direct(SimBucket::new());
        let origin = Origin { parent: test_branch(1), at: 7 };
        let point = test_branch(3);
        let made = Child { branch: point, owner: 0x33, point: true };
        block_on(record_fork(
            &bucket,
            made,
            origin,
            crate::keys::Parents::Kms(&crate::keys::testing::host_kms(), None),
        ))
        .unwrap();
        let theirs = |id| Branch { cell: layout::Cell { tenant: 2, id: 1 }, id };
        let fork = |child: Branch| block_on(fork_point(&bucket, child, point, origin, child.id));
        let record = |branch| block_on(record(&bucket, branch)).unwrap();

        assert_eq!(fork(theirs(4)), Err(Error::NotShared(point)));
        assert_eq!(fork(test_branch(5)), Ok(()));
        assert_eq!(block_on(share(&bucket, point, true)), Ok(()));
        assert_eq!(block_on(share(&bucket, point, true)), Ok(()), "shared again");
        assert_eq!(record(point), Some(Record { shared: true, ..Kind::Point(origin).of(0x33) }));
        assert_eq!(fork(theirs(6)), Ok(()));
        assert_eq!(block_on(share(&bucket, point, false)), Ok(()));
        assert_eq!(fork(theirs(7)), Err(Error::NotShared(point)));
        assert_eq!(record(point), Some(Kind::Point(origin).of(0x33)));
        assert_eq!(record(theirs(6)), Some(Kind::Clone(origin).of(6)));
        assert_eq!(record(test_branch(5)), Some(Kind::Clone(origin).of(5)));

        let refused = block_on(share(&bucket, test_branch(5), true));
        assert_eq!(refused, Err(Error::NotPoint(test_branch(5))));
        assert_eq!(block_on(share(&bucket, test_branch(8), true)), Err(Error::NoBranch));
        block_on(mark_deleted(&bucket, point)).unwrap();
        assert_eq!(block_on(share(&bucket, point, true)), Err(Error::Deleted));
    }

    /// TASK-79.82's last case: two forks name one child within a round trip, neither's segment
    /// landing, and each takes a record of its own for it. One takes it; the other finds another's,
    /// though the bytes of what each wrote differ only by their owners, and deleting its fork
    /// leaves the first's be.
    #[test]
    fn forks_to_one_child_take_one_record() {
        let bucket = Direct(SimBucket::new());
        assert_eq!(block_on(create_forked(&bucket, test_branch(2), 0x21)), Ok(()));
        assert_eq!(
            block_on(create_forked(&bucket, test_branch(2), 0x21)),
            Ok(()),
            "its own, again"
        );
        assert_eq!(
            block_on(create_forked(&bucket, test_branch(2), 0x22)),
            Err(Error::IdTaken(test_branch(2)))
        );
        assert_eq!(
            block_on(delete_fork(
                &bucket,
                test_branch(2),
                Kind::Own,
                0x22,
                (9, 0),
                &crate::keys::testing::host_kms()
            )),
            Ok(false)
        );
        let taken = Kind::Own.of(0x21);
        assert_eq!(block_on(record(&bucket, test_branch(2))), Ok(Some(taken.clone())));
        assert_eq!(
            block_on(delete_fork(
                &bucket,
                test_branch(2),
                Kind::Own,
                0x21,
                (9, 0),
                &crate::keys::testing::host_kms()
            )),
            Ok(true)
        );
        assert_eq!(
            block_on(record(&bucket, test_branch(2))),
            Ok(Some(Record { deleted: true, ..taken }))
        );
    }

    /// TASK-160.14: a fork of a branch at a segment and a fork of a point made at that segment,
    /// which names it as its own origin, given one child ID, as guests may give any: whichever
    /// record lands first, the other fork finds it another's, though the two name one origin and
    /// one ID, and deleting that fork leaves it be. So they were, until records named their owners
    /// (TASK-153.9): with the rule that they do off, each takes the other's for its own.
    #[test]
    fn forks_and_forks_of_points_at_one_segment_take_no_records_of_each_other() {
        use crate::{Rules, set_rules};
        let origin = Origin { parent: test_branch(1), at: 7 };
        let (point, direct, of_point) =
            (3, Child { branch: test_branch(2), owner: 0x21, point: false }, 0x22);
        // What each fork gave, the direct first or not, and whose the child's record is.
        let race = |direct_first: bool| {
            let bucket = Direct(SimBucket::new());
            let made = Child { branch: test_branch(point), owner: 0x33, point: true };
            block_on(record_fork(
                &bucket,
                made,
                origin,
                crate::keys::Parents::Kms(&crate::keys::testing::host_kms(), None),
            ))
            .unwrap();
            let fork = || {
                block_on(record_fork(
                    &bucket,
                    direct,
                    origin,
                    crate::keys::Parents::Kms(&crate::keys::testing::host_kms(), None),
                ))
            };
            let restore = || {
                block_on(fork_point(&bucket, test_branch(2), test_branch(point), origin, of_point))
            };
            let (forked, restored, lost, at) = if direct_first {
                let forked = fork();
                (forked, restore(), of_point, Kind::Forking { origin, point: test_branch(point) })
            } else {
                let restored = restore();
                (fork(), restored, direct.owner, Kind::Fork(origin))
            };
            // The fork that lost is deleted, as its host's cleanup deletes it.
            let deleted = block_on(delete_fork(
                &bucket,
                test_branch(2),
                at,
                lost,
                (9, 0),
                &crate::keys::testing::host_kms(),
            ));
            let owner =
                block_on(record(&bucket, test_branch(2))).unwrap().map(|record| record.owner);
            (forked, restored, deleted, owner)
        };
        let taken = Err(Error::IdTaken(test_branch(2)));
        assert_eq!(race(true), (Ok(()), taken.clone(), Ok(false), Some(direct.owner)));
        assert_eq!(race(false), (taken, Ok(()), Ok(false), Some(of_point)));

        set_rules(Rules { records_owned: false, ..Rules::ALL });
        // The loser's deletion then deletes the winner's branch, of a parent not there.
        let unowned = |direct_first| {
            let (forked, restored, _, owner) = race(direct_first);
            (forked, restored, owner)
        };
        let unowned = (unowned(true), unowned(false));
        set_rules(Rules::ALL);
        let both = (Ok(()), Ok(()), Some(0));
        assert_eq!(unowned, (both.clone(), both), "each takes the other's record");
    }

    /// TASK-160.14: whatever made a branch's record, whatever else makes one for its ID, of
    /// another owner, as guests may give any ID, finds it another's, and changes nothing of it:
    /// a create, a fork's child taking a branch of its own, a fork's writers recording its child,
    /// a point or not, a fork of a point, and deleting a fork, wherever it was made.
    #[test]
    fn makers_of_one_id_take_nothing_of_each_others() {
        let (origin, point, child) = (Origin { parent: test_branch(1), at: 7 }, 3, 2);
        let said = |result: &dyn std::fmt::Debug| format!("{result:?}");
        let fork = |owner, point| Child { branch: test_branch(child), owner, point };
        let unfork = |bucket: &Direct, at, owner| {
            block_on(delete_fork(
                bucket,
                test_branch(child),
                at,
                owner,
                (9, 0),
                &crate::keys::testing::host_kms(),
            ))
        };
        // Each maker, of the owner it is given, and what it gives, in words, finding another's
        // record, live. The first five make live records; deleting a fork leaves none.
        type Maker<'a> = (&'a str, &'a dyn Fn(&Direct, u64) -> String, &'a str);
        let makers: [Maker; 9] = [
            (
                "create",
                &|bucket, owner| said(&block_on(create(bucket, test_branch(child), owner))),
                "Err(Corrupt(\"branch 1/1/2 exists\"))",
            ),
            (
                "create_forked",
                &|bucket, owner| said(&block_on(create_forked(bucket, test_branch(child), owner))),
                "Err(IdTaken(Branch { cell: Cell { tenant: 1, id: 1 }, id: 2 }))",
            ),
            (
                "record_fork",
                &|bucket, owner| {
                    said(&block_on(record_fork(
                        bucket,
                        fork(owner, false),
                        origin,
                        crate::keys::Parents::Kms(&crate::keys::testing::host_kms(), None),
                    )))
                },
                "Err(IdTaken(Branch { cell: Cell { tenant: 1, id: 1 }, id: 2 }))",
            ),
            (
                "record_fork of a point",
                &|bucket, owner| {
                    said(&block_on(record_fork(
                        bucket,
                        fork(owner, true),
                        origin,
                        crate::keys::Parents::Kms(&crate::keys::testing::host_kms(), None),
                    )))
                },
                "Err(IdTaken(Branch { cell: Cell { tenant: 1, id: 1 }, id: 2 }))",
            ),
            (
                "fork_point",
                &|bucket, owner| {
                    said(&block_on(fork_point(
                        bucket,
                        test_branch(child),
                        test_branch(point),
                        origin,
                        owner,
                    )))
                },
                "Err(IdTaken(Branch { cell: Cell { tenant: 1, id: 1 }, id: 2 }))",
            ),
            (
                "delete_fork, nowhere",
                &|bucket, owner| said(&unfork(bucket, Kind::Own, owner)),
                "Ok(false)",
            ),
            (
                "delete_fork, at a segment",
                &|bucket, owner| said(&unfork(bucket, Kind::Fork(origin), owner)),
                "Ok(false)",
            ),
            (
                "delete_fork, to a point",
                &|bucket, owner| said(&unfork(bucket, Kind::Point(origin), owner)),
                "Ok(false)",
            ),
            (
                "delete_fork, of a point",
                &|bucket, owner| {
                    said(&unfork(
                        bucket,
                        Kind::Forking { origin, point: test_branch(point) },
                        owner,
                    ))
                },
                "Ok(false)",
            ),
        ];
        for (first, make, _) in &makers[..5] {
            for (second, again, another) in &makers {
                let bucket = Direct(SimBucket::new());
                block_on(record_fork(
                    &bucket,
                    Child { branch: test_branch(point), owner: 0x33, point: true },
                    origin,
                    crate::keys::Parents::Kms(&crate::keys::testing::host_kms(), None),
                ))
                .unwrap();
                assert_eq!(make(&bucket, 0x11), "Ok(())", "{first}");
                let made = |bucket: &Direct| {
                    let record = block_on(record(bucket, test_branch(child))).unwrap();
                    (record, block_on(head::epoch(bucket, test_branch(child))))
                };
                let before = made(&bucket);
                assert_eq!(again(&bucket, 0x12), *another, "{second} after {first}");
                assert_eq!(made(&bucket), before, "{second} after {first}");
            }
        }
    }

    /// TASK-79.82: a branch that takes a failed fork's child's ID after the fork's deletion found
    /// it free, before the deletion takes the child's head, is left be: its record taken and its
    /// head kept, not the deletion's.
    #[test]
    fn forks_deleted_leave_a_branch_taking_their_childs_id_meanwhile() {
        use crate::bucket::{ByteRange, Error as BucketError, Meta, Object, PutMode, Version};
        use std::future::Future;
        use std::sync::atomic::{AtomicBool, Ordering};

        /// A bucket in which branch `child` is created, its head taken, as its record is first
        /// found missing.
        struct Racing {
            bucket: Direct,
            child: Branch,
            raced: AtomicBool,
        }

        impl Bucket for Racing {
            fn get(&self, key: &str) -> impl Future<Output = Result<Object, BucketError>> + Send {
                let key = key.to_owned();
                async move {
                    let got = self.bucket.get(&key).await;
                    if key == layout::record(self.child)
                        && matches!(got, Err(BucketError::NotFound))
                        && !self.raced.swap(true, Ordering::SeqCst)
                    {
                        create(&self.bucket, self.child, 0).await.unwrap();
                        head::take(&self.bucket, self.child, 7, 0).await.unwrap();
                    }
                    got
                }
            }

            fn get_range(
                &self,
                key: &str,
                range: ByteRange,
            ) -> impl Future<Output = Result<Vec<u8>, BucketError>> + Send {
                self.bucket.get_range(key, range)
            }

            fn head(&self, key: &str) -> impl Future<Output = Result<Meta, BucketError>> + Send {
                self.bucket.head(key)
            }

            fn list(
                &self,
                prefix: &str,
            ) -> impl Future<Output = Result<Vec<Meta>, BucketError>> + Send {
                self.bucket.list(prefix)
            }

            fn list_delimited(
                &self,
                prefix: &str,
                page: crate::bucket::Page<'_>,
            ) -> impl Future<Output = Result<crate::bucket::Listing, crate::bucket::Error>> + Send
            {
                self.bucket.list_delimited(prefix, page)
            }

            fn put(
                &self,
                key: &str,
                bytes: bytes::Bytes,
                mode: PutMode,
            ) -> impl Future<Output = Result<Version, BucketError>> + Send {
                self.bucket.put(key, bytes, mode)
            }

            fn delete(&self, key: &str) -> impl Future<Output = Result<(), BucketError>> + Send {
                self.bucket.delete(key)
            }

            fn now(&self) -> u64 {
                self.bucket.now()
            }
        }

        let racing = Racing {
            bucket: Direct(SimBucket::new()),
            child: test_branch(3),
            raced: AtomicBool::new(false),
        };
        let origin = Origin { parent: test_branch(1), at: 7 };
        assert_eq!(
            block_on(delete_fork(
                &racing,
                test_branch(3),
                Kind::Fork(origin),
                0,
                (9, 0),
                &crate::keys::testing::host_kms()
            )),
            Ok(false)
        );
        let record = block_on(record(&racing.bucket, test_branch(3))).unwrap();
        assert_eq!(record, Some(Kind::Root.of(0)));
        let head = block_on(head::get(&racing.bucket, test_branch(3))).unwrap().unwrap();
        assert_eq!((head.epoch, head.holder), (1, 7), "the other branch's head");
    }

    /// TASK-79.82: a parent whose state names a fork to an ID another branch took is deleted all
    /// the same, the other branch left as it is.
    #[test]
    fn parents_are_deleted_though_another_branch_took_their_forks_id() {
        use crate::batch::{Commit, SegmentBuilder};
        use crate::writer::Writer;
        let memory = std::sync::Arc::new(object_store::memory::InMemory::new());
        let bucket = std::sync::Arc::new(crate::object_bucket::ObjectBucket::new(memory, ""));
        block_on(async {
            create(&*bucket, test_branch(1), 0).await.unwrap();
            create(&*bucket, test_branch(2), 0).await.unwrap();
            let parent = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
            let forking = SegmentBuilder::from(Commit { tag: 1, ..Commit::default() });
            let at = parent
                .add_segment(
                    forking
                        .fork(child(test_branch(2)), &mut parent.chain())
                        .frame(parent.keys().encrypting()),
                )
                .unwrap();
            parent.upload(at).await.unwrap();
            assert_eq!(parent.acknowledge().await, Ok(Some(at)));
            assert!(parent.refused(test_branch(2)));
            assert_eq!(
                delete(&*bucket, test_branch(1), 20, 0, &crate::keys::testing::host_kms()).await,
                Ok(())
            );
            assert_eq!(record(&*bucket, test_branch(2)).await, Ok(Some(Kind::Root.of(0))));
        });
    }

    /// A failed fork's claim landing, as a dead host's write may, once a pass of the collector
    /// took what it names, nothing having kept that: at 2 of the parent's, in fact the fence of its
    /// next writer, its fork's segment void. It holds up no pass, and its deletion finishes. Found
    /// by the simulation, seed 3184 of its `restores`.
    #[test]
    fn claims_landing_after_what_they_name_went_hold_up_nothing() {
        use crate::batch::{Commit, SegmentBuilder};
        use crate::collector::{Options, collect};
        use crate::writer::Writer;
        let memory = std::sync::Arc::new(object_store::memory::InMemory::new());
        let bucket = std::sync::Arc::new(crate::object_bucket::ObjectBucket::new(memory, ""));
        const NOW: Options = Options { compact_below: 0.0, grace: 0 };
        block_on(async {
            create(&*bucket, test_branch(1), 0).await.unwrap();
            let parent = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
            for tag in 1..=3 {
                let commit = SegmentBuilder::from(Commit { tag, ..Commit::default() });
                let segment = parent
                    .add_segment(
                        commit.close(&mut parent.chain()).frame(parent.keys().encrypting()),
                    )
                    .unwrap();
                parent.upload(segment).await.unwrap();
                parent.acknowledge().await.unwrap();
            }
            parent.checkpoint().await.unwrap();
            collect(&*bucket, 1, &NOW).await.unwrap();
            let origin = Origin { parent: test_branch(1), at: 1 };
            let lost = "branch 1/1/3: what its recovery starts from keeps going".to_owned();
            assert_eq!(
                crate::recovery::recover(&*bucket, test_branch(3), Some(origin)).await.map(drop),
                Err(Error::Lost(lost))
            );

            let claim = Kind::Claim(origin).of(0);
            let (key, bytes) = (layout::record(test_branch(3)), layout::encode(&claim));
            assert!(matches!(bucket::create(&*bucket, &key, bytes).await, Created::Created(_)));
            collect(&*bucket, 2, &NOW).await.unwrap();
            let deleted = Record { deleted: true, ..claim };
            assert_eq!(record(&*bucket, test_branch(3)).await, Ok(Some(deleted)));

            // Its deletion, which stopped, finds it so; or, the pass not there first, finishes it.
            let claim = Kind::Claim(origin).of(0);
            let (key, bytes) = (layout::record(test_branch(4)), layout::encode(&claim));
            assert!(matches!(bucket::create(&*bucket, &key, bytes).await, Created::Created(_)));
            assert_eq!(
                delete_fork(
                    &*bucket,
                    test_branch(4),
                    Kind::Fork(origin),
                    0,
                    (9, 0),
                    &crate::keys::testing::host_kms()
                )
                .await,
                Ok(true)
            );
            assert_eq!(
                record(&*bucket, test_branch(4)).await,
                Ok(Some(Record { deleted: true, ..claim }))
            );

            // But one a writer has lost something: it stops the pass, as any branch's.
            let claim = Kind::Claim(origin).of(0);
            let (key, bytes) = (layout::record(test_branch(5)), layout::encode(&claim));
            assert!(matches!(bucket::create(&*bucket, &key, bytes).await, Created::Created(_)));
            head::take(&*bucket, test_branch(5), 11, 0).await.unwrap();
            let lost = "branch 1/1/5: what its recovery starts from keeps going".to_owned();
            assert_eq!(collect(&*bucket, 3, &NOW).await, Err(Error::Lost(lost)));
            assert_eq!(record(&*bucket, test_branch(5)).await, Ok(Some(claim)));
        });
    }

    /// A branch deleted, before it had a record, has a deleted one, and its head is taken no more.
    #[test]
    fn deleted_branches_take_no_head() {
        let bucket = Direct(SimBucket::new());
        let taken = block_on(head::take(&bucket, test_branch(3), 1, 0)).unwrap();
        assert_eq!(taken.head.epoch, 1);
        block_on(delete(&bucket, test_branch(3), 2, 0, &crate::keys::testing::host_kms())).unwrap();
        let record = block_on(record(&bucket, test_branch(3))).unwrap();
        // Of the deletion's owner, its holder's, which no other's is.
        let deleted = Record { deleted: true, ..Kind::Own.of(2) };
        assert_eq!(record, Some(deleted));
        let taken = block_on(head::take(&bucket, test_branch(3), 3, 0));
        assert!(matches!(taken, Err(Error::Deleted)), "{taken:?}");
    }

    /// TASK-79.78: a fork deleted is deleted again, which does nothing, though a pass of the
    /// collector deleted what recovering it read, its parent's state where it was forked, which no
    /// live branch needs: so a deletion that stopped once the branch was deleted here, its host
    /// crashing before what else keeps the branch went, finishes, deleted again. Here the parent
    /// frees what the fork inherited, and checkpoints past it.
    #[test]
    fn forks_deleted_are_deleted_again_once_what_they_inherited_went() {
        use crate::batch::{Commit, SegmentBuilder};
        use crate::writer::Writer;
        use storage_device::ExtentId;
        let memory = std::sync::Arc::new(object_store::memory::InMemory::new());
        let bucket = std::sync::Arc::new(crate::object_bucket::ObjectBucket::new(memory, ""));
        block_on(async {
            create(&*bucket, test_branch(1), 0).await.unwrap();
            let parent = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
            let extent = ExtentId::new(1, parent.chain().next_sequence());
            let extents = vec![(extent, b"inherited".to_vec())];
            let forking = SegmentBuilder::from(Commit { tag: 1, extents, ..Commit::default() });
            let at = parent
                .add_segment(
                    forking
                        .fork(child(test_branch(2)), &mut parent.chain())
                        .frame(parent.keys().encrypting()),
                )
                .unwrap();
            parent.upload(at).await.unwrap();
            parent.acknowledge().await.unwrap();
            let freeing =
                SegmentBuilder::from(Commit { tag: 2, frees: vec![extent], ..Commit::default() });
            let segment = parent
                .add_segment(freeing.close(&mut parent.chain()).frame(parent.keys().encrypting()))
                .unwrap();
            parent.upload(segment).await.unwrap();
            parent.acknowledge().await.unwrap();
            parent.checkpoint().await.unwrap();
            delete(&*bucket, test_branch(2), 20, 0, &crate::keys::testing::host_kms())
                .await
                .unwrap();
            let now = crate::collector::Options { compact_below: 0.0, grace: 0 };
            crate::collector::collect(&*bucket, 1, &now).await.unwrap();
            // What recovering the fork reads went with the pass.
            let origin = Origin { parent: test_branch(1), at };
            let lost = "branch 1/1/2: what its recovery starts from keeps going".to_owned();
            assert_eq!(
                crate::recovery::recover(&*bucket, test_branch(2), Some(origin)).await.map(drop),
                Err(Error::Lost(lost))
            );

            assert_eq!(
                delete(&*bucket, test_branch(2), 21, 0, &crate::keys::testing::host_kms()).await,
                Ok(())
            );
            let deleted = Record { deleted: true, ..Kind::Fork(origin).of(0) };
            assert_eq!(record(&*bucket, test_branch(2)).await, Ok(Some(deleted)));
        });
    }
}
