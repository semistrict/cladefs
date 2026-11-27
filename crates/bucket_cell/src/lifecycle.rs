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
use crate::head::{self, Read};
#[cfg(feature = "full")]
use crate::keys::Parents;
#[cfg(feature = "full")]
use crate::layout::Origin;
use crate::layout::{self, CellId, Head};
#[cfg(feature = "full")]
use crate::record::Made;
#[cfg(feature = "full")]
use crate::recovery;
use crate::{Error, rules};
use futures::StreamExt as _;
use serde::Serialize;
#[cfg(feature = "full")]
use std::collections::BTreeSet;

/// What a [`transition`] makes of the object it found.
pub(crate) enum Step<T, R> {
    /// Writes `T` in place of what it found, if that is still there, and gives `R`.
    Write(T, R),
    /// Writes nothing, and gives `R`.
    Stop(R),
}

/// What a [`transition`] starts from: of an object, or of a cell's head ([`head::Read`]).
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
    start: Start<Read>,
    step: impl FnMut(Option<Head>) -> Result<Step<Head, R>, Error>,
) -> Result<R, Error> {
    Ok(change_leaving(bucket, cell, start, step).await?.0)
}

/// Changes `cell`'s head as [`change`] does, with the head it left.
async fn change_leaving<R>(
    bucket: &impl Bucket,
    cell: CellId,
    start: Start<Read>,
    step: impl FnMut(Option<Head>) -> Result<Step<Head, R>, Error>,
) -> Result<(R, Read), Error> {
    head::change_leaving(bucket, cell, start, step).await
}

/// Creates `cell`, of no parent, of `owner`, which its maker chose at random, never taken: its
/// first taking gives it its keys and kind ([`head::take`]). Returns whether it made it: a head
/// there already, whoever's, being made or not, is left as it is.
#[cfg(feature = "full")]
pub async fn create(bucket: &impl Bucket, cell: CellId, owner: u64) -> Result<bool, Error> {
    let head = Head::of(Made::Root, owner);
    change(bucket, cell, Start::Create, |found| match found {
        None => Ok(Step::Write(head.clone(), true)),
        Some(_) => Ok(Step::Stop(false)),
    })
    .await
}

/// cladefs: writes the reference of the fork of `origin` to `child`, whose owner is `owner`, under
/// the parent's cell ([`layout::reference`], TASK-395.3): create-only, its bytes the fork's alone
/// ([`layout::Reference`]), so that every writer of it, and a write landed though its answer was
/// lost, writes the same. One there already stands, whatever its bytes: a pass keeps by its key.
///
/// cladefs: in the child's bucket, if the version's mirror is marked there; else in the parent's
/// (doc-23, Forks; TASK-633.8). Where it went.
#[cfg(feature = "full")]
async fn refer(
    bucket: &impl Bucket,
    origin: Origin,
    child: CellId,
    owner: u64,
) -> Result<Referred, Error> {
    let key = layout::reference(origin.parent, origin.at, child);
    let bytes = layout::encode(&layout::Reference::of(owner));
    if let Some(here) = bucket.mirrors()
        && here.marked(origin.parent, origin.at).await?
    {
        here.create(&key, bytes).await?;
        return Ok(Referred { owner, here: true });
    }
    // A fork stopped here read no marker, and refers in the parent's bucket once it goes on, its
    // version's mirror marked meanwhile or not (a test's).
    failpoint!("version::unmarked");
    bucket::create(bucket, &key, bytes).await?;
    Ok(Referred { owner, here: false })
}

/// cladefs: where a fork's reference went ([`refer`]): `here`, in the child's bucket, its version's
/// mirror marked there; else in the parent's.
#[cfg(feature = "full")]
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
#[cfg(feature = "full")]
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
#[cfg(feature = "full")]
async fn retire_one(bucket: &impl Bucket, key: &str) -> Result<(), bucket::Refused> {
    if rules().retirements_marked
        && let Some(mark) = layout::retired_of(key)
    {
        mark_retired(bucket, &mark).await?;
    }
    bucket::delete(bucket, key).await
}

/// cladefs: whether the host of incarnation `incarnation` was decommissioned, as the bucket of
/// `parent`, where its ephemeral cells forked from `parent` have their references, says: its
/// tombstone there ([`layout::decommissioned`], board 8519). Its decommissioning writes one in
/// every bucket before it retires anything, and buckets added later copy the root's tombstones
/// before their identity is written (board 9966, hotbed question 543).
pub async fn decommissioned(
    bucket: &impl Bucket,
    incarnation: u64,
    parent: CellId,
) -> Result<bool, bucket::Refused> {
    let tombstone = layout::decommissioned(layout::ephemeral_bucket(parent), incarnation);
    Ok(bucket::head(bucket, &tombstone).await?.is_some())
}

#[cfg(feature = "full")]
mod full_ops {
    use super::*;

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
    /// ([`layout::ephemeral_index`]), both in the bucket of `origin`'s parent, so that decommissioning
    /// the incarnation finds every reference of its; then reads the incarnation's tombstone there
    /// ([`decommissioned`], board 8519): found, the incarnation decommissioned, both are deleted again
    /// and the reference refused ([`Error::Decommissioned`]), so that no reference stands of an
    /// incarnation decommissioned, which nothing would retire. A decommissioning writes the tombstone
    /// before it lists the entries: a reference whose reading misses it was written before that
    /// listing, which finds it.
    pub async fn refer_ephemeral(
        bucket: &impl Bucket,
        origin: Origin,
        incarnation: u64,
        child: CellId,
    ) -> Result<(), Error> {
        let index = layout::ephemeral_index(incarnation, origin.parent, origin.at, child);
        bucket::create(bucket, &index, Vec::new()).await?;
        let reference = layout::ephemeral_reference(origin.parent, origin.at, incarnation, child);
        bucket::create(bucket, &reference, Vec::new()).await?;
        if decommissioned(bucket, incarnation, origin.parent).await? {
            unrefer_ephemeral(bucket, origin, incarnation, child).await?;
            return Err(Error::Decommissioned(incarnation));
        }
        Ok(())
    }

    /// cladefs: deletes the reference of `child`, an ephemeral cell of the host of incarnation
    /// `incarnation`, to `origin`, then its index entry: its cell gone, or never made, or its host
    /// gone for good ([`decommission`]). Both in the bucket of `origin`'s parent, as
    /// [`refer_ephemeral`] wrote them, whichever bucket `bucket` is the home of.
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

    /// cladefs: the numbers of the buckets an ephemeral cell's reference may be in, as `bucket`'s
    /// root says (board 8517): every bucket its copy of the registry records, its own, and
    /// [`crate::identity::NAMED`], the number of the cells tests name, which stay in the bucket asked.
    /// `crate::routed::RoutedBucket::ephemeral_buckets` adds those the root's registry records.
    pub async fn ephemeral_buckets(bucket: &impl Bucket) -> Result<BTreeSet<u16>, Error> {
        let mut numbers = BTreeSet::from([crate::identity::NAMED]);
        numbers.extend(crate::identity::get(bucket).await?.map(|identity| identity.number));
        let registry = crate::identity::registry(bucket).await?;
        numbers.extend(
            registry.iter().flat_map(|registry| &registry.buckets).map(|entry| entry.number),
        );
        Ok(numbers)
    }

    /// cladefs: the references of the ephemeral cells of the host of incarnation `incarnation`, by
    /// their index in each of the buckets numbered `buckets` (doc-25, board 8517): each child, with
    /// its origin. One listing a bucket.
    /// cladefs: refused, [`bucket::Refused`] (board 5861).
    pub async fn ephemeral_references(
        bucket: &impl Bucket,
        buckets: &BTreeSet<u16>,
        incarnation: u64,
    ) -> Result<Vec<(Origin, CellId)>, bucket::Refused> {
        let mut references = Vec::new();
        for &number in buckets {
            let listed =
                bucket::list(bucket, &layout::ephemeral_indexes(number, incarnation)).await?;
            references.extend(
                (listed.iter())
                    .filter_map(|meta| layout::ephemeral_indexed(&meta.key))
                    .filter(|&(of, ..)| of == incarnation)
                    .map(|(_, parent, at, child)| (Origin { parent, at, epoch: 0 }, child)),
            );
        }
        Ok(references)
    }

    /// cladefs: what the buckets numbered `buckets` hold of a host's ephemeral cells (board 8521).
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct EphemeralHost {
        /// How many references of its ephemeral cells their index names.
        pub references: usize,
        /// Whether it was decommissioned, as any of those buckets' tombstones says.
        pub decommissioned: bool,
    }

    /// cladefs: every incarnation of a host whose ephemeral cells have references in the buckets
    /// numbered `buckets`, or that was decommissioned there, by their index and tombstones (board
    /// 8521): for its operator, without the host's disk or log, to find the incarnation of a host lost
    /// and decommission it. One listing a bucket.
    pub async fn ephemeral_hosts(
        bucket: &impl Bucket,
        buckets: &BTreeSet<u16>,
    ) -> Result<std::collections::BTreeMap<u64, EphemeralHost>, bucket::Refused> {
        let mut hosts = std::collections::BTreeMap::<u64, EphemeralHost>::new();
        for &number in buckets {
            for meta in bucket::list(bucket, &layout::ephemeral_hosts(number)).await? {
                if let Some((incarnation, ..)) = layout::ephemeral_indexed(&meta.key) {
                    hosts.entry(incarnation).or_default().references += 1;
                } else if let Some((_, incarnation)) = layout::decommissioned_of(&meta.key) {
                    hosts.entry(incarnation).or_default().decommissioned = true;
                }
            }
        }
        Ok(hosts)
    }

    /// cladefs: the segments of `cell` that what refers to it under its prefix refers to
    /// ([`layout::references`]): forks', ephemeral cells', and mirrors' pins and holds, each a state
    /// of the cell another cell may read, which its writer relocates nothing of (doc-27, Choosing
    /// what to relocate). One LIST.
    /// cladefs: refused, [`bucket::Refused`] (board 5861).
    pub async fn referenced_at(
        bucket: &impl Bucket,
        cell: CellId,
    ) -> Result<BTreeSet<u64>, bucket::Refused> {
        let listed = bucket::list(bucket, &layout::references(cell)).await?;
        Ok((listed.iter())
            .filter_map(|meta| match layout::kind(&meta.key) {
                layout::Kind::Reference { parent, at, .. }
                | layout::Kind::Ephemeral { parent, at, .. }
                | layout::Kind::Pin { parent, at }
                | layout::Kind::Hold { parent, at, .. }
                    if parent == cell =>
                {
                    Some(at)
                }
                _ => None,
            })
            .collect())
    }

    /// cladefs: what a call of [`decommission`] did, and left.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Decommissioned {
        /// The references it retired, each with its index entry.
        pub retired: usize,
        /// The references it found and left, past the most it was asked to retire: a call again
        /// retires them.
        pub left: usize,
    }

    /// cladefs: re-reads the authoritative set of buckets recorded in the registry: through
    /// `Bucket::ephemeral_buckets` when routed (which reads the root bucket's registry directly),
    /// else [`ephemeral_buckets`].
    async fn reread_ephemeral_buckets(bucket: &impl Bucket) -> Result<BTreeSet<u16>, Error> {
        match bucket.ephemeral_buckets() {
            Some(fut) => fut.await,
            None => ephemeral_buckets(bucket).await,
        }
    }

    /// cladefs: decommissions the host of incarnation `incarnation`, gone for good, its disk lost
    /// (doc-25, TASK-698.3; specs/ephemeral), in the buckets numbered `buckets`, every bucket a
    /// reference of its may be in ([`ephemeral_buckets`], board 8517): its tombstone written in each
    /// first ([`layout::decommissioned`], board 8519), so that no host of it refers to anything there
    /// from then on; then the root registry re-read so any bucket added concurrently (whose
    /// `identity::add` listed root tombstones before our root write) gets its tombstone and is
    /// listed too (board 9966, hotbed question 543); then the references of its ephemeral cells,
    /// which no host holds any more, `most` of them at most, deleted in batches
    /// ([`bucket::delete_all`], board 8523), then their index entries: stopped between, the entries
    /// left find the references a call again deletes, as deleting what is gone is no error. Any
    /// in-flight reference PUT from the lost host that lands after its index entry is deleted here
    /// is retired by the collector by this tombstone (board 9968, hotbed question 544). Returns how
    /// many it retired, and left. Its operator's to call, for an incarnation no host has: a host of
    /// it still holding its cells would read what it no longer keeps, and refuses to start, or fork,
    /// once it reads the tombstone.
    pub async fn decommission(
        bucket: &impl Bucket,
        buckets: &BTreeSet<u16>,
        incarnation: u64,
        most: usize,
    ) -> Result<Decommissioned, Error> {
        let mut all = buckets.clone();
        for &number in &all {
            bucket::create(bucket, &layout::decommissioned(number, incarnation), Vec::new())
                .await?;
        }
        // cladefs: Dekker barrier with `identity::add` on the root bucket (board 9966, hotbed
        // question 543): `identity::add` writes `root/buckets` then lists `root`'s tombstones,
        // while we write `root`'s tombstone then re-read `root/buckets`. If `ROOT` was absent from
        // the caller's initial set and appears now, write `ROOT`'s tombstone and re-read once more
        // so the barrier holds for every concurrent addition.
        let mut reread = reread_ephemeral_buckets(bucket).await?;
        if !all.contains(&crate::identity::ROOT) && reread.contains(&crate::identity::ROOT) {
            bucket::create(
                bucket,
                &layout::decommissioned(crate::identity::ROOT, incarnation),
                Vec::new(),
            )
            .await?;
            all.insert(crate::identity::ROOT);
            reread = reread_ephemeral_buckets(bucket).await?;
        }
        for number in reread.into_iter().filter(|&number| crate::identity::given(number)) {
            if all.insert(number) {
                bucket::create(bucket, &layout::decommissioned(number, incarnation), Vec::new())
                    .await?;
            }
        }
        let references = ephemeral_references(bucket, &all, incarnation).await?;
        let (retiring, left) = references.split_at(most.min(references.len()));
        let referred: Vec<String> = (retiring.iter())
            .map(|(origin, child)| {
                layout::ephemeral_reference(origin.parent, origin.at, incarnation, *child)
            })
            .collect();
        bucket::delete_all(bucket, &referred.iter().map(String::as_str).collect::<Vec<_>>())
            .await?;
        failpoint!("ephemeral::decommission_unreferred");
        let indexed: Vec<String> = (retiring.iter())
            .map(|(origin, child)| {
                layout::ephemeral_index(incarnation, origin.parent, origin.at, *child)
            })
            .collect();
        bucket::delete_all(bucket, &indexed.iter().map(String::as_str).collect::<Vec<_>>()).await?;
        Ok(Decommissioned { retired: retiring.len(), left: left.len() })
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

    /// cladefs: [`fork_version`], the child's head made with `inheriting`, if any: a KEK for the
    /// child's cell, its first DEK and those it inherits, rewrapped under that KEK
    /// ([`crate::keys::Inheriting::spend`], TASK-411.4), so that its first taking gives it none.
    pub async fn fork_version_keyed(
        bucket: &impl Bucket,
        child: CellId,
        version: u64,
        origin: Origin,
        (owner, inheriting): (u64, Option<&crate::keys::Inheriting>),
        keys: Parents<'_>,
    ) -> Result<(), Error> {
        failpoint!("version::record");
        // cladefs: before creating a child head (which would embed parent DEKs wrapped under the
        // child's KEK), verify that a cross-tenant fork targets an existing, unshredded, shared
        // version (TASK-544, board 2484). Same-tenant forks still write `Made::Forking` before reading
        // the parent head so they linearize with concurrent `drop_versions` (doc-18); `finish_fork`
        // re-checks after the head and reference land.
        let crosses =
            origin.parent.tenant != child.tenant && origin.parent.tenant != layout::DEFAULT_TENANT;
        if crosses {
            let parent = head::get(bucket, origin.parent).await?;
            if parent.as_ref().is_some_and(|head| head.shredded) {
                return Err(crate::keys::shredded(origin.parent));
            }
            let named = parent
                .as_ref()
                .and_then(|head| head.version(version))
                .filter(|named| named.at == origin.at && named.epoch == origin.epoch);
            if rules().forks_check_versions && named.is_none() {
                return Err(Error::Deleted);
            }
            if !named.is_some_and(|named| named.shared) {
                return Err(Error::NotShared(origin.parent));
            }
        }
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
        if let Some(inheriting) = inheriting {
            let given = inheriting.spend();
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
        let (making, left) = making?;
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
        start: Start<Read>,
        referred: Option<Referred>,
    ) -> Result<(), Error> {
        let cell = origin.parent;
        // Into another tenant's cell, a version of the default tenant's, or one shared.
        let crosses = cell.tenant != child.tenant && cell.tenant != layout::DEFAULT_TENANT;
        if rules().forks_check_versions || crosses {
            let head = head::get(bucket, cell).await?;
            // cladefs: a shredded cell's versions can never be forked into new cells, even on a host
            // that was never told of the shred (TASK-545, board 2486, board 2704).
            if head.as_ref().is_some_and(|head| head.shredded) {
                return Err(crate::keys::shredded(cell));
            }
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
            match recovery::checkpoint_at(bucket, cell, origin.at, keys).await {
                Ok(()) => {}
                // cladefs: given up on meanwhile ([`abandon`]), its maker slow, and its version
                // dropped: nothing keeps what it would read, which the collector may have deleted
                // since, its cell's writer relocating out of it (doc-27). Failed as the fork given up
                // on is, not as the version's state lost.
                // Only so: given up on, and its version no longer named, else what it reads is kept
                // (the verifying review of d33a6c360).
                Err(lost @ Error::Lost(_)) => {
                    let given_up =
                        head::get(bucket, child).await?.is_none_or(|head| head.deleted());
                    let parent = head::get(bucket, cell).await?;
                    let named = parent.as_ref().and_then(|head| head.version(version)).is_some();
                    return Err(if given_up && !named { Error::Deleted } else { lost });
                }
                Err(error) => return Err(error),
            }
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
        .await?;
        if made {
            failpoint!("version::made");
        }
        Ok(())
    }
}

#[cfg(feature = "full")]
pub use full_ops::{
    Decommissioned, EphemeralHost, decommission, ephemeral_buckets, ephemeral_hosts,
    ephemeral_references, fork_ephemeral, fork_version, fork_version_keyed, refer_ephemeral,
    referenced_at, unrefer_ephemeral,
};

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
/// (doc-18, TASK-476), and for a `Made::Forking` head that never became a clone, its DEKs and
/// inherited DEKs (TASK-544, board 2484).
fn ended(head: Head, holder: u64) -> Head {
    let (deks, inherited) = match head.forking() {
        Some(_) => (Vec::new(), Vec::new()),
        None => (head.deks, head.inherited),
    };
    Head {
        epoch: Head::FINAL,
        holder,
        token: u64::MAX,
        versions: Vec::new(),
        deks,
        inherited,
        fast_wal: None,
        ..head
    }
}

/// Deletes `cell`, as `holder`, of the placement whose fencing token is `token`: one swap of its
/// head to its final epoch, which no writer takes again, the one it has stopping at its next swap,
/// and the collector reclaims what no other cell inherited from it. Deleting it again does
/// nothing, though the collector reclaimed that since. A cell with no head, a fork's child before
/// its head or no cell, gets one deleted from the first ([`Made::Own`]), its owner the deletion's,
/// which no other's is. Fails for a cell a later placement took, whose token is higher
/// ([`Error::Stale`]), leaving it as it was: a placement cut off deletes no cell from under the one
/// after it (TASK-45.1).
#[cfg(feature = "full")]
pub async fn delete(
    bucket: &impl Bucket,
    cell: CellId,
    holder: u64,
    token: u64,
) -> Result<(), Error> {
    delete_if(bucket, cell, (holder, token), holder, |_| true).await.map(drop)
}

/// cladefs: ends `cell`'s head if it currently exists and is live, returning `true` if this call
/// transitioned it to deleted (`Head::FINAL`). Never creates a head for a missing cell (board
/// 8511), making it safe for collector-driven tenant reaping and `--delete-tenant`.
pub async fn delete_existing(
    bucket: &impl Bucket,
    cell: CellId,
    holder: u64,
    token: u64,
) -> Result<bool, Error> {
    let deleted = change(bucket, cell, Start::Read, |found| match found {
        None => Ok(Step::Stop(false)),
        Some(found) if found.deleted() => Ok(Step::Stop(false)),
        Some(found) if rules().delete_tokens && found.token > token => Err(Error::Stale),
        Some(found) => Ok(Step::Write(ended(found, holder), true)),
    })
    .await?;
    if deleted {
        failpoint!("lifecycle::head_ended");
    }
    Ok(deleted)
}

/// Deletes `cell` as [`delete`] does, if `ours` says its head is one to delete, one with none
/// getting a head of `owner`'s: returns whether it is deleted, now or before, whoever's; `false`
/// for a head left as it is.
#[cfg(feature = "full")]
async fn delete_if(
    bucket: &impl Bucket,
    cell: CellId,
    (holder, token): (u64, u64),
    owner: u64,
    ours: impl Fn(&Head) -> bool,
) -> Result<bool, Error> {
    let none = ended(Head::of(Made::Own, owner), holder);
    let deleted = change(bucket, cell, Start::Read, |found| match found {
        None => Ok(Step::Write(none.clone(), true)),
        Some(found) if found.deleted() => Ok(Step::Stop(true)),
        Some(found) if !ours(&found) => Ok(Step::Stop(false)),
        Some(found) if rules().delete_tokens && found.token > token => Err(Error::Stale),
        Some(found) => Ok(Step::Write(ended(found, holder), true)),
    })
    .await?;
    failpoint!("lifecycle::head_ended");
    Ok(deleted)
}

/// Deletes `child`, the child of a fork of a version, as [`delete`] does, but only if its cell is
/// the one the fork made: its head names the fork's `owner`, or it has none yet, which gets one
/// deleted from the first, for good, so that a create of its head landing late, a dead host's,
/// finds it so. A cell of another's, whose ID the child was given, is left as it is (TASK-79.82);
/// so is a cell deleted already, whoever's. Returns whether the fork's cell is gone, deleted now
/// or before: `false` for another's.
#[cfg(feature = "full")]
pub async fn delete_fork(
    bucket: &impl Bucket,
    child: CellId,
    owner: u64,
    (holder, token): (u64, u64),
) -> Result<bool, Error> {
    let ours = |found: &Head| found.is_child_of(owner);
    delete_if(bucket, child, (holder, token), owner, ours).await
}

#[cfg(all(test, feature = "full"))]
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
        let deleted =
            Head { epoch: Head::FINAL, holder: 2, token: u64::MAX, ..Head::of(Made::Own, 2) };
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

    /// The counts of the ephemeral cells' references, index entries and tombstones `bucket` holds.
    fn ephemeral_kinds(bucket: &impl Bucket) -> (usize, usize, usize) {
        let listed = block_on(bucket::list(bucket, "")).unwrap();
        let count = |of: fn(&layout::Kind) -> bool| {
            listed.iter().filter(|meta| of(&layout::kind(&meta.key))).count()
        };
        (
            count(|kind| matches!(kind, layout::Kind::Ephemeral { .. })),
            count(|kind| matches!(kind, layout::Kind::EphemeralIndex { .. })),
            count(|kind| matches!(kind, layout::Kind::Decommissioned { .. })),
        )
    }

    /// Board 8523: a decommissioning retires its `most` references in batches, references first,
    /// then their index entries: stopped between (its failpoint), it leaves the entries, which a
    /// call again finds and finishes, deleting what is gone being no error; each call retires its
    /// most, the rest left and said. Board 8519: its tombstone, written first, stays, and a host
    /// of the incarnation refers to nothing from then on: its reference taken back, refused.
    #[test]
    fn decommissionings_stopped_part_way_are_finished_again() {
        let scenario = fail::FailScenario::setup();
        let bucket = Direct(SimBucket::new());
        let buckets = block_on(ephemeral_buckets(&bucket)).unwrap();
        assert_eq!(buckets, BTreeSet::from([crate::identity::NAMED]));
        let (origin, incarnation) = (Origin { parent: test_cell(1), at: 7, epoch: 1 }, 0x2b);
        for child in 10..15 {
            block_on(refer_ephemeral(&bucket, origin, incarnation, test_cell(child))).unwrap();
        }
        assert_eq!(ephemeral_kinds(&bucket), (5, 5, 0));

        fail::cfg("ephemeral::decommission_unreferred", "return").unwrap();
        let stopped = block_on(decommission(&bucket, &buckets, incarnation, 3));
        let failpoint = "failpoint ephemeral::decommission_unreferred".to_owned();
        assert_eq!(stopped, Err(Error::Read(failpoint)));
        assert_eq!(ephemeral_kinds(&bucket), (2, 5, 1), "its references gone, their entries left");
        fail::remove("ephemeral::decommission_unreferred");
        let again = block_on(decommission(&bucket, &buckets, incarnation, 3));
        assert_eq!(again, Ok(Decommissioned { retired: 3, left: 2 }));
        assert_eq!(ephemeral_kinds(&bucket), (2, 2, 1));
        let rest = block_on(decommission(&bucket, &buckets, incarnation, 3));
        assert_eq!(rest, Ok(Decommissioned { retired: 2, left: 0 }));
        assert_eq!(ephemeral_kinds(&bucket), (0, 0, 1));

        let late = block_on(refer_ephemeral(&bucket, origin, incarnation, test_cell(20)));
        assert_eq!(late, Err(Error::Decommissioned(incarnation)));
        assert_eq!(ephemeral_kinds(&bucket), (0, 0, 1), "taken back");
        let other = block_on(refer_ephemeral(&bucket, origin, incarnation + 2, test_cell(20)));
        assert_eq!(other, Ok(()), "another incarnation's refers on");
        scenario.teardown();
    }

    /// Board 8523's measurement: 2,000 ephemeral references of a host decommissioned against a
    /// bucket whose every request waits 25 ms (its faults' latency), on tokio's paused clock, so
    /// that what is measured is the requests' latency alone: deleted a request pair a reference,
    /// in series, as decommissioning did, and batched, as it does. Printed for the task's reply,
    /// and the batched at least twenty times the faster.
    #[tokio::test(start_paused = true)]
    async fn decommissionings_delete_in_batches() {
        use crate::faults::{Faults, Faulty};
        use futures::StreamExt as _;
        const REFERENCES: u64 = 2_000;
        let faults = Faults::new(std::time::Duration::from_millis(25), None);
        let objects = std::sync::Arc::new(object_store::memory::InMemory::new());
        let faulty = std::sync::Arc::new(Faulty::new(objects, faults));
        let bucket = crate::object_bucket::ObjectBucket::new(faulty, "");
        let buckets = BTreeSet::from([crate::identity::NAMED]);
        let origin = Origin { parent: test_cell(1), at: 7, epoch: 1 };
        let (serial, batched) = (0x2b, 0x2d);
        for incarnation in [serial, batched] {
            let referring = (0..REFERENCES)
                .map(|child| refer_ephemeral(&bucket, origin, incarnation, test_cell(100 + child)));
            let referred: Vec<Result<(), Error>> =
                futures::stream::iter(referring).buffer_unordered(64).collect().await;
            referred.into_iter().collect::<Result<Vec<()>, Error>>().unwrap();
        }

        let start = tokio::time::Instant::now();
        let references = ephemeral_references(&bucket, &buckets, serial).await.unwrap();
        for (origin, child) in references {
            unrefer_ephemeral(&bucket, origin, serial, child).await.unwrap();
        }
        let one_by_one = start.elapsed();
        let start = tokio::time::Instant::now();
        let decommissioned = decommission(&bucket, &buckets, batched, usize::MAX).await;
        let in_batches = start.elapsed();
        assert_eq!(decommissioned, Ok(Decommissioned { retired: REFERENCES as usize, left: 0 }));
        println!(
            "{REFERENCES} references at 25 ms a request: one by one {one_by_one:?}, in batches \
             {in_batches:?}"
        );
        assert!(in_batches * 20 < one_by_one, "{in_batches:?} against {one_by_one:?}");
    }
}
