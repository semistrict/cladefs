// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Cells' records: creating a cell, forking one, taking one over, deleting one.
//!
//! A fork is of a version: its child's record alone, which writes nothing of the version's cell's,
//! so that any number of hosts fork one at once ([`fork_version`]).
//!
//! cladefs: every change of a record, a cell's object or a collector lease is a [`transition`],
//! one compare-and-swap loop, of what it found to what it writes (TASK-160.14).

use crate::bucket::{self, Bucket, Created, Object, Updated};
use crate::head;
use crate::keys::Parents;
use crate::layout::{self, CellId, Head, Origin};
use crate::record::{Kind, Record};
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
/// nothing or what it writes lands. Every change of a cell's record, a cell's object or a
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

/// Changes `cell`'s record as `step` says ([`transition`]).
async fn change<R>(
    bucket: &impl Bucket,
    cell: CellId,
    start: Start,
    step: impl FnMut(Option<Record>) -> Result<Step<Record, R>, Error>,
) -> Result<R, Error> {
    let decode = |bytes: &[u8]| Record::decode(cell, bytes);
    transition(bucket, &layout::record(cell), start, decode, step).await
}

/// Changes `cell`'s record as [`change`] does, with the record object it left
/// ([`transition_leaving`]).
async fn change_leaving<R>(
    bucket: &impl Bucket,
    cell: CellId,
    start: Start,
    step: impl FnMut(Option<Record>) -> Result<Step<Record, R>, Error>,
) -> Result<(R, Option<Object>), Error> {
    let decode = |bytes: &[u8]| Record::decode(cell, bytes);
    transition_leaving(bucket, &layout::record(cell), start, decode, step).await
}

/// Creates `cell`, of no parent, of `owner`, which its maker chose at random: a create of
/// another's finds its record there.
pub async fn create(bucket: &impl Bucket, cell: CellId, owner: u64) -> Result<(), Error> {
    let record = Kind::Root.of(owner);
    change(bucket, cell, Start::Create, |found| match found {
        None => Ok(Step::Write(record.clone(), ())),
        Some(_) => Err(Error::Corrupt(format!("cell {cell} exists"))),
    })
    .await
}

/// cladefs: gives `cell`'s record the DEKs `deks` says it should have, its own and those it
/// inherits, given what it finds there, by a conditional update ([`transition`]); none, it is left
/// as it is (TASK-411.3, TASK-411.4, [`crate::keys`]). The writer's of the cell alone, holding
/// its cell, or its fork's host, before any store of it opens. Returns the record as it left it:
/// [`Error::NoCell`] if it has none, [`Error::Deleted`] if it was deleted.
pub async fn give_deks(
    bucket: &impl Bucket,
    cell: CellId,
    deks: impl FnMut(&Record) -> Option<(Vec<crate::keys::Dek>, Vec<crate::keys::InheritedDek>)>,
) -> Result<Record, Error> {
    give_deks_from(bucket, cell, Start::Read, deks).await
}

/// [`give_deks`], from `found`, `cell`'s record as a read before found it, raw: so that a fork's
/// host reads it as it makes the child's cell, a round trip less (TASK-411.4).
pub(crate) async fn give_deks_found(
    bucket: &impl Bucket,
    cell: CellId,
    found: Option<Object>,
    deks: impl FnMut(&Record) -> Option<(Vec<crate::keys::Dek>, Vec<crate::keys::InheritedDek>)>,
) -> Result<Record, Error> {
    give_deks_from(bucket, cell, Start::Found(found), deks).await
}

async fn give_deks_from(
    bucket: &impl Bucket,
    cell: CellId,
    start: Start,
    mut deks: impl FnMut(&Record) -> Option<(Vec<crate::keys::Dek>, Vec<crate::keys::InheritedDek>)>,
) -> Result<Record, Error> {
    change(bucket, cell, start, |found| match found {
        None => Err(Error::NoCell),
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

/// `cell`'s record, if it has one: [`Error::Format`] if it is of another format than this one,
/// whose objects this reads none of.
pub async fn record(bucket: &impl Bucket, cell: CellId) -> Result<Option<Record>, Error> {
    match bucket::get(bucket, &layout::record(cell)).await {
        Some(object) => Ok(Some(Record::decode(cell, &object.bytes)?)),
        None => Ok(None),
    }
}

/// `cell`'s record, if it says the cell is live: an operator's look, to compare with what hosts
/// hold (`cladefs-gc --cells`, TASK-395.4). A fork whose child has no record yet isn't live.
pub async fn live(bucket: &impl Bucket, cell: CellId) -> Result<Option<Record>, Error> {
    Ok(record(bucket, cell).await?.filter(|record| !record.deleted))
}

/// cladefs: writes the reference of the fork of `origin` to `child`, whose owner is `owner`, under
/// the parent's cell ([`layout::reference`], TASK-395.3): create-only, its bytes the fork's alone
/// ([`layout::Reference`]), so that every writer of it, and a write landed though its answer was
/// lost, writes the same. One there already stands, whatever its bytes: a pass keeps by its key.
async fn refer(bucket: &impl Bucket, origin: Origin, child: CellId, owner: u64) {
    let key = layout::reference(origin.parent, origin.at, child);
    bucket::create(bucket, &key, layout::encode(&layout::Reference::of(owner))).await;
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
/// another origin than the fork's, or none (TASK-395.10): the fork made no cell, and no record will
/// name it, so that no pass would retire it ([`crate::collector`] retires a reference by the origin
/// its child's deleted record names). Marked ([`retire`]).
async fn take_back(bucket: &impl Bucket, origin: Origin, child: CellId, found: &Record) {
    if rules().references && found.origin() != Some(origin) {
        retire_one(bucket, &layout::reference(origin.parent, origin.at, child)).await;
    }
}

/// cladefs: a restore of the version `version` of the cell `origin` names, at the segment it
/// names, to `child`, whose owner is `owner` (doc-18, TASK-476): the child's record made, being
/// made ([`Kind::Forking`]), then its reference to its origin, then the cell's head read, which
/// must name the version there ([`Error::Deleted`] otherwise, the version dropped or the cell
/// deleted), shared if `child` is of another tenant's cell ([`Error::NotShared`]), then the fork
/// made, a clone ([`Kind::Clone`]). Forked again, its record being made, it resumes the fork as
/// another would ([`resume_version_fork`]). For a child of another cell than
/// the cell's, the cell's checkpoint at the version's segment too, before the fork is made,
/// made if none is there, sealed under `keys` (`recovery::checkpoint_at`): the child's recovery
/// reads its parent across cells by that key alone, and only the first restore into another cell
/// writes it. Five round trips, the checkpoint there; the first such restore reads the cell's
/// state at the version and writes the checkpoint besides.
pub async fn fork_version(
    bucket: &impl Bucket,
    child: CellId,
    version: u64,
    origin: Origin,
    owner: u64,
    keys: Parents<'_>,
) -> Result<(), Error> {
    let nothing = std::future::ready(());
    fork_version_with(bucket, child, version, origin, owner, keys, nothing).await.map(drop)
}

/// cladefs: [`fork_version`], with `alongside` run at once with the reference's write, once the
/// child's record is made, if this makes the fork: its host's holding of the child's cell, which
/// the child's store would hold first as it opens (TASK-449). Returns, if this made the fork, the
/// child's record as it left it, a clone made; and what `alongside` gave, if it ran.
pub async fn fork_version_with<T>(
    bucket: &impl Bucket,
    child: CellId,
    version: u64,
    origin: Origin,
    owner: u64,
    keys: Parents<'_>,
    alongside: impl Future<Output = T>,
) -> Result<(Option<Record>, Option<T>), Error> {
    failpoint!("version::record");
    let refers = rules().references && rules().clones_refer_first;
    // Without the rule, as a test shows it needed: the reference before the record.
    let refers_first = refers && !rules().clones_record_first;
    if refers_first {
        refer(bucket, origin, child, owner).await;
        failpoint!("version::referred");
    }
    let kind = match rules().forks_pending {
        true => Kind::Forking { origin, version },
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
        // Ours, given up on, its version dropped.
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
        failpoint!("version::recorded");
        let referring = async {
            if refers && !refers_first {
                refer(bucket, origin, child, owner).await;
                failpoint!("version::referred");
            }
            Ok::<(), Error>(())
        };
        let (referred, beside) = futures::join!(referring, alongside);
        referred?;
        let start = Start::Found(left);
        made = (finish_fork(bucket, child, version, origin, keys, start).await?, Some(beside));
    }
    // Without the rule, as a test shows it needed: once made.
    if rules().references && !rules().clones_refer_first {
        refer(bucket, origin, child, owner).await;
    }
    Ok(made)
}

/// cladefs: resumes the fork of a version to `child` that another began ([`fork_version`]), its record
/// saying it is being made: a store opening `child`, its host dead or slow, makes it, so that it
/// takes no state of a fork not made (TASK-455's review). The record is said resumed first
/// ([`Record::resumed`]), with a condition, so that the clone made says so, whose reference's
/// retirement is then marked: the host that began it, slow, may write the reference after the
/// clone was made, deleted and its cell removed (rule `resumes_marked`). Then the reference, as
/// that host writes it, before the fork is made (rule `resumes_refer`): it may have died first.
/// Then the fork made, as [`fork_version`] makes it. A fork made already, it writes nothing: its
/// host, or a resume, wrote the reference before. Fails as [`fork_version`] does, and with
/// [`Error::Deleted`] for a fork given up on, [`Error::NoCell`] for no record. A restore into
/// another cell writes the checkpoint at the version's segment, if none is there, sealed under
/// `keys`.
pub async fn resume_version_fork(
    bucket: &impl Bucket,
    child: CellId,
    keys: Parents<'_>,
) -> Result<(), Error> {
    let forking = change(bucket, child, Start::Read, |found| match found {
        None => Err(Error::NoCell),
        Some(found) if found.deleted => Err(Error::Deleted),
        Some(found) if found.forking().is_none() => Ok(Step::Stop(None)),
        Some(found) if found.resumed => Ok(Step::Stop(Some(found))),
        Some(found) => Ok(Step::Write(found.clone().resumed(), Some(found))),
    })
    .await?;
    let Some(Record { kind: Kind::Forking { origin, version }, owner, .. }) = forking else {
        return Ok(());
    };
    // Without the rule, as a test shows it needed: none written.
    if rules().references && rules().clones_refer_first && rules().resumes_refer {
        refer(bucket, origin, child, owner).await;
    }
    finish_fork(bucket, child, version, origin, keys, Start::Read).await.map(drop)
}

/// The fork being made of the version `version`, of `origin`, to `child`, made, the child's
/// record's update starting from `start`: the record as its create left it, a read less
/// (TASK-449), its update conditional on that version as on one a read found, so that its
/// deletion, its giving up, or a resume since makes it read the record anew. The version's
/// cell's head read first, and for a child of another cell, the cell's checkpoint at the
/// version made, sealed under `keys` (TASK-476). Returns the record as it left it, if it made the
/// fork.
async fn finish_fork(
    bucket: &impl Bucket,
    child: CellId,
    version: u64,
    origin: Origin,
    keys: Parents<'_>,
    start: Start,
) -> Result<Option<Record>, Error> {
    let cell = origin.parent;
    if rules().forks_check_versions || cell.tenant != child.tenant {
        let head = head::get(bucket, cell).await?;
        let named = head.as_ref().and_then(|head| head.version(version));
        let named = named.filter(|named| named.at == origin.at);
        if rules().forks_check_versions && named.is_none() {
            return Err(Error::Deleted);
        }
        if cell.tenant != child.tenant && !named.is_some_and(|named| named.shared) {
            return Err(Error::NotShared(cell));
        }
    }
    if cell != child {
        recovery::checkpoint_at(bucket, cell, origin.at, keys).await?;
    }
    failpoint!("version::checked");
    // Conditional, against its being deleted, or given up on ([`abandon`]). A record's bytes
    // repeat only for the same change, which is then made.
    let made = change(bucket, child, start, |found| match found {
        None => Err(Error::NoCell),
        Some(found) if found.deleted => Err(Error::Deleted),
        Some(found @ Record { kind: Kind::Forking { origin, .. }, .. }) => {
            let made = Record { kind: Kind::Clone(origin), ..found };
            Ok(Step::Write(made.clone(), Some(made)))
        }
        Some(_) => Ok(Step::Stop(None)),
    })
    .await?;
    if made.is_some() {
        failpoint!("version::made");
    }
    Ok(made)
}

/// Gives up on `child`, a fork being made of a version that was deleted ([`Kind::Forking`]), so
/// that nothing keeps what it names, unless the fork was made first: returns its record as it then
/// stands. Its fork, which fails then, wrote nothing: a cell that holds nothing and that nothing
/// relies on, what it names perhaps gone, which recovering it would stop the collector on.
pub(crate) async fn abandon(bucket: &impl Bucket, child: CellId) -> Result<Record, Error> {
    change(bucket, child, Start::Read, |found| match found {
        None => Err(Error::NoCell),
        Some(found) if found.deleted || found.forking().is_none() => Ok(Step::Stop(found)),
        Some(found) => {
            let abandoned = Record { deleted: true, ..found };
            Ok(Step::Write(abandoned.clone(), abandoned))
        }
    })
    .await
}

/// Deletes `cell`, as `holder`, of the placement whose fencing token is `token`: no writer takes
/// it again, the one it has stops at its next swap of the head, and the collector reclaims what no
/// other cell inherited from it. Deleting it again does nothing, though the collector reclaimed
/// that since. Fails for a cell a later placement took, whose token is higher ([`Error::Stale`]),
/// leaving it as it was: a placement cut off deletes no cell from under the one after it
/// (TASK-45.1).
pub async fn delete(
    bucket: &impl Bucket,
    cell: CellId,
    holder: u64,
    token: u64,
) -> Result<(), Error> {
    end_head(bucket, cell, holder, token).await?;
    failpoint!("lifecycle::head_ended");
    // A fork's child before its record, or no cell: either way none is, from now, unless its
    // record came meanwhile. Its owner the deletion's, which no other's is.
    let deleted = Record { deleted: true, ..Kind::Own.of(holder) };
    change(bucket, cell, Start::Read, |found| match found {
        None => Ok(Step::Write(deleted.clone(), ())),
        Some(found) if found.deleted => Ok(Step::Stop(())),
        Some(found) => Ok(Step::Write(Record { deleted: true, ..found }, ())),
    })
    .await
}

/// Ends `cell`'s head, as [`delete`] does first: its final epoch, which no writer takes, as
/// `holder` of the placement whose fencing token is `token`, unless a later placement took it
/// ([`Error::Stale`]). Returns it, ended: what it names, kept.
async fn end_head(
    bucket: &impl Bucket,
    cell: CellId,
    holder: u64,
    token: u64,
) -> Result<Head, Error> {
    let read = head::read(bucket, cell).await;
    // cladefs: ended already, by this deletion or another, whatever placement's: deletions are
    // idempotent. Else the token is resolved by the cell (TASK-364).
    if let Some(found) = read.head()?
        && found.epoch == Head::FINAL
    {
        return Ok(found);
    }
    let token = deleting_token(bucket, cell, token).await?;
    head::end(bucket, cell, read, holder, token).await
}

/// cladefs: the token a deletion by the placement of `token` ends `cell`'s head with, as its
/// writers take it with their cell's epoch (TASK-364): the cell's epoch now, which takes the head
/// from its holder's writer, and from none a later taking of the cell made; refused if a later
/// placement holds the cell ([`Error::Stale`]). The deletion takes nothing of the cell, its other
/// cells' writers going on. `token` itself where the cell has no object, or was deleted, as then
/// no writer took the cell's head by its cell; and the highest token there is, an operator's,
/// whichever placement took the cell last.
async fn deleting_token(bucket: &impl Bucket, cell: CellId, token: u64) -> Result<u64, Error> {
    if token == u64::MAX {
        return Ok(token);
    }
    Ok(match crate::cell::read(bucket, cell).await? {
        Some(cell) if cell.deleted => token,
        Some(cell) if rules().delete_tokens && cell.token > token => return Err(Error::Stale),
        Some(cell) => cell.epoch,
        None => token,
    })
}

/// Deletes `child`, the child of a fork of a version, as [`delete`] does, but only if its cell is
/// the one the fork made: its record names the fork's `owner`, or it has none yet. A cell of
/// another's, whose ID the child was given, is left as it is (TASK-79.82); so is a cell deleted
/// already, whoever's. Returns whether the fork's cell is gone, deleted now or before: `false` for
/// another's. A child with no record yet, its fork never made, holds nothing: it gets a record
/// deleted from the first, for good, created only if none is there, so that a create of its record
/// landing late, a dead host's, finds it so; then its head is ended.
pub async fn delete_fork(
    bucket: &impl Bucket,
    child: CellId,
    owner: u64,
    (holder, token): (u64, u64),
) -> Result<bool, Error> {
    // What deleting the child, whose record is `record`, gives.
    let delete_found = async |record: Record| match record {
        Record { deleted: true, .. } => Ok(true),
        record if !record.is_child_of(owner) => Ok(false),
        _ => delete(bucket, child, holder, token).await.map(|()| true),
    };
    if let Some(record) = record(bucket, child).await? {
        return delete_found(record).await;
    }
    let record = Record { deleted: true, ..Kind::Own.of(owner) };
    let taken = change(bucket, child, Start::Create, |taken| match taken {
        None => Ok(Step::Write(record.clone(), None)),
        Some(taken) => Ok(Step::Stop(Some(taken))),
    })
    .await?;
    if let Some(taken) = taken {
        return delete_found(taken).await;
    }
    failpoint!("lifecycle::claimed");
    end_head(bucket, child, holder, token).await?;
    failpoint!("lifecycle::claim_head_ended");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::test_cell;
    use crate::testing::{Direct, SimBucket};
    use futures::executor::block_on;

    /// TASK-79.82: a fork's child is deleted only if its cell is the fork's: one whose record names
    /// the fork's owner, or one with none, which gets a record deleted from the first. One whose
    /// record names another owner is left as it is. One deleted is gone already.
    #[test]
    fn forks_delete_only_their_own_cells() {
        let bucket = Direct(SimBucket::new());
        let live = |cell| block_on(record(&bucket, cell)).unwrap().map(|record| !record.deleted);
        let unfork = |cell, owner| block_on(delete_fork(&bucket, cell, owner, (9, 0)));
        // A clone's, made.
        let origin = Origin { parent: test_cell(1), at: 7 };
        let made = Kind::Clone(origin).of(0x33);
        block_on(bucket::create(&bucket, &layout::record(test_cell(3)), layout::encode(&made)));
        assert_eq!(unfork(test_cell(3), 0x33), Ok(true));
        assert_eq!(live(test_cell(3)), Some(false));
        assert_eq!(unfork(test_cell(3), 0x33), Ok(true), "gone already");
        // One with no record yet.
        assert_eq!(unfork(test_cell(4), 0x44), Ok(true));
        assert_eq!(live(test_cell(4)), Some(false));
        // Another's.
        block_on(create(&bucket, test_cell(5), 0x55)).unwrap();
        assert_eq!(unfork(test_cell(5), 0x50), Ok(false));
        assert_eq!(live(test_cell(5)), Some(true));
        let taken = block_on(head::take(&bucket, test_cell(5), 15, 0));
        assert_eq!(taken.map(|held| held.head.epoch), Ok(1));
    }

    /// TASK-476: a restore of a version is made while its cell's head names it there, into a cell
    /// of its tenant's or, shared, of any; refused once it is dropped, or its cell deleted, its
    /// child's record then deleted by its host. Into another cell than its cell's, it writes the
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
        let origin = Origin { parent: cell, at };
        let celled = |tenant, id| CellId { tenant, id };
        let restore = |child: CellId, id| {
            let kms = crate::keys::testing::host_kms();
            block_on(fork_version(&*bucket, child, id, origin, child.id, Parents::Kms(&kms, None)))
        };
        let record = |cell| block_on(record(&*bucket, cell)).unwrap();
        let checkpoint = layout::checkpoint(cell, at, writer.epoch());
        let checkpointed = || block_on(bucket::get(&*bucket, &checkpoint)).is_some();

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
            record(celled(1, 7)).map(|record| record.kind),
            Some(Kind::Forking { origin, version: 0xa })
        );
        assert_eq!(record(celled(2, 6)), Some(Kind::Clone(origin).of(6)), "made, it stays");
    }

    /// TASK-160.14: whatever made a cell's record, whatever else makes one for its ID, of
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
        // record, live. The first two make live records; deleting a fork leaves none.
        type Maker<'a> = (&'a str, &'a dyn Fn(&Direct, u64) -> String, &'a str);
        let taken = "Err(IdTaken(CellId { tenant: 1, id: 2 }))";
        let makers: [Maker; 3] = [
            (
                "create",
                &|bucket, owner| said(&block_on(create(bucket, test_cell(child), owner))),
                "Err(Corrupt(\"cell 1/2 exists\"))",
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
                let made = |bucket: &Direct| {
                    let record = block_on(record(bucket, test_cell(child))).unwrap();
                    (record, block_on(head::epoch(bucket, test_cell(child))))
                };
                let before = made(&bucket);
                assert_eq!(again(&bucket, 0x12), *another, "{second} after {first}");
                assert_eq!(made(&bucket), before, "{second} after {first}");
            }
        }
    }

    /// A cell deleted, before it had a record, has a deleted one, and its head is taken no more.
    #[test]
    fn deleted_cells_take_no_head() {
        let bucket = Direct(SimBucket::new());
        let taken = block_on(head::take(&bucket, test_cell(3), 1, 0)).unwrap();
        assert_eq!(taken.head.epoch, 1);
        block_on(delete(&bucket, test_cell(3), 2, 0)).unwrap();
        let record = block_on(record(&bucket, test_cell(3))).unwrap();
        // Of the deletion's owner, its holder's, which no other's is.
        let deleted = Record { deleted: true, ..Kind::Own.of(2) };
        assert_eq!(record, Some(deleted));
        let taken = block_on(head::take(&bucket, test_cell(3), 3, 0));
        assert!(matches!(taken, Err(Error::Deleted)), "{taken:?}");
    }
}
