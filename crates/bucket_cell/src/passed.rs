// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a cell's collector lease (`<C>gc/lease`, [`layout::collector_lease`]): which pass holds
//! the cell, for how long, and what the last pass over it to end recorded for the passes after it,
//! and what the cell takes in the bucket (doc-7, Garbage collection; TASK-395.5, TASK-395.6,
//! TASK-395.8). One object, so that a pass learns all of it by the read that takes the lease.
//!
//! Every pass over the cell takes the lease before anything else ([`take`]): unless another pass
//! holds it, unexpired, or ended since the taker's sweep began, when the taker leaves the cell to
//! it ([`Error::Left`], [`Holding`]). A pass holding its cell for a time renews the lease between
//! its steps and lets it go as it ends ([`change`]); a pass of a process gone holds it until it
//! expires. A taking fences every pass before it, as a cell's taking of its head does: a pass
//! fenced deletes nothing, and writes nothing to the lease.
//!
//! What it records goes with every taking to the next ([`Passed`]): a pass that ran to its end
//! records, as it lets the lease go, the digest of its listing of the cell, but the collector's
//! lease (`<C>gc/lease`), and the cell's objects and bytes, by kind, as of its listing ([`Usage`]).
//!
//! A pass whose listing has the digest recorded decides what the recorded one did: everything it
//! decides by is under the cell's prefix, and an object there written, rewritten or deleted since
//! changes the listing, or it was recorded with no digest ([`Passed::listing`]). It deletes nothing
//! (doc-7 has why that is as safe as a pass in full). Who recorded it, of what process, is of no
//! matter: the lease's holders and times are in the lease, outside the digest.

use crate::bucket::{self, Bucket, Meta};
use crate::collector::Holding;
use crate::layout::{self, CellId, FORMAT};
use crate::{Error, Left, lifecycle};
use fprint::TypeFingerprint;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;

/// A cell's collector lease ([`layout::collector_lease`]): the format of the bucket's objects,
/// first, as a cell's object says it; the epoch of its last taking, and who took it, as a cell's
/// head does ([`layout::Head`]); until when that pass holds the cell, and when the last pass to go
/// through ended; and what the last pass over the cell that ran to its end recorded, carried from
/// each taking to the next ([`Passed`]). Time is the backend's clock as each pass read it
/// ([`Bucket::now`]), in milliseconds, never a host's own (board 8471): every host's reading of it
/// is at most behind the backend's, never ahead, so that a lease ends, and a pass's end orders
/// against a sweep's start, by one clock for all of them, a host whose own clock runs fast
/// starving no other's sweeps nor holding its cells past their time. A pass taking a cell early,
/// as one whose clock is ahead would, fences the pass holding it, which then deletes nothing:
/// never wrong, only late.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub struct CollectorLease {
    pub format: u32,
    pub epoch: u64,
    /// Who took it: unique to each taking, so that a write of it is recognised.
    pub holder: u64,
    /// Until when the pass holds the cell, renewed as it goes; 0 once it ended, or if it holds it
    /// for no time (TASK-395.5). A pass of a process gone holds it until then.
    pub until: u64,
    /// When the last pass over the cell that went through, holding it for a time, ended: a sweep
    /// that began before leaves the cell to it ([`Left::Swept`]). 0 if none did.
    pub ended: u64,
    pub passed: Option<Passed>,
}

impl CollectorLease {
    /// The lease of `cell` `bytes` encode: [`Error::Format`] if it is of an unsupported format.
    pub fn decode(cell: CellId, bytes: &[u8]) -> Result<Self, Error> {
        let format: u32 = layout::decode(bytes.get(..4).unwrap_or(bytes))?;
        let format = FORMAT.check(format).map_err(|_| Error::Format { cell, format })?;
        match format {
            31.. => layout::decode(bytes),
            _ => unreachable!("FORMAT.check enforces oldest..=current"),
        }
    }

    /// Whether a pass of `holder`, holding its cell as `holding` says, leaves the cell to another,
    /// as of `now`: one holding it, unexpired ([`Left::Held`]), or one that went through since the
    /// taker's sweep began ([`Left::Swept`]).
    pub(crate) fn leaves(&self, holder: u64, holding: Holding, now: u64) -> Option<Left> {
        if self.holder != holder && self.until > now {
            return Some(Left::Held);
        }
        holding.since.is_some_and(|since| self.ended > since).then_some(Left::Swept)
    }

    /// Itself, its usage counting it as it is encoded: one object of the bytes it takes. Its
    /// encoding's length is the same whatever numbers it holds (`bincode`'s are of fixed width), so
    /// counting itself changes it not.
    fn counting_itself(mut self) -> Self {
        loop {
            let len = layout::encode(&self).len() as u64;
            let Some(passed) = &mut self.passed else { return self };
            let itself = Sum { objects: 1, bytes: len };
            let counted = passed.usage.kinds.entry(GC_LEASE.to_owned()).or_default();
            if *counted == itself {
                return self;
            }
            *counted = itself;
        }
    }
}

/// The name of the kind of a cell's collector lease ([`layout::Kind::name`]).
const GC_LEASE: &str = "gc-lease";

/// What a pass over a cell that ran to its end recorded ([`CollectorLease::passed`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub struct Passed {
    /// The digest of the listing of the cell it decided by, but the collector's lease
    /// ([`Listed::digest`]): what a pass listing the same decides as it did. None if it decided
    /// by more than the cell holds, which may change with no object of the cell's changing: a
    /// version of another cell's record, read for a clone of it being made; an origin found lost; a
    /// cell's recovery that found what it needs lost, and then nothing lost.
    pub listing: Option<[u8; 32]>,
    /// The cell's objects and bytes, as of its listing, less what it deleted.
    pub usage: Usage,
}

impl Passed {
    /// Whether a pass that listed the cell as `listed` decides as this one did.
    pub(crate) fn holds_for(&self, listed: &Listed) -> bool {
        self.listing == Some(listed.digest)
    }
}

/// What a cell takes in the bucket, as a pass's listing found it ([`Passed::usage`],
/// TASK-395.8).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub struct Usage {
    /// When the listing it is of began, by the backend's clock: milliseconds since the Unix epoch
    /// on a real backend.
    pub measured: u64,
    /// Objects, and their bytes as stored, by kind ([`layout::Kind::name`]): the cell's
    /// objects, its collector's lease and marks, and the references of
    /// forks of its cells; the garbage among them the collector has yet to reclaim too.
    pub kinds: BTreeMap<String, Sum>,
}

impl Usage {
    /// Counts the object at `key`, of `size` bytes.
    pub(crate) fn add(&mut self, key: &str, size: u64) {
        let sum = self.kinds.entry(layout::kind(key).name().to_owned()).or_default();
        sum.objects += 1;
        sum.bytes += size;
    }

    /// Counts the object at `key`, of `size` bytes, no longer.
    pub(crate) fn remove(&mut self, key: &str, size: u64) {
        let name = layout::kind(key).name();
        let Some(sum) = self.kinds.get_mut(name) else { return };
        sum.objects = sum.objects.saturating_sub(1);
        sum.bytes = sum.bytes.saturating_sub(size);
        if sum.objects == 0 {
            self.kinds.remove(name);
        }
    }

    /// Its objects and bytes, of every kind.
    pub fn total(&self) -> Sum {
        let mut total = Sum::default();
        self.kinds.values().for_each(|sum| total += *sum);
        total
    }
}

/// Objects, and their bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub struct Sum {
    pub objects: u64,
    pub bytes: u64,
}

impl std::ops::AddAssign for Sum {
    fn add_assign(&mut self, other: Self) {
        self.objects += other.objects;
        self.bytes += other.bytes;
    }
}

/// What a pass's listing of a cell found.
pub(crate) struct Listed {
    /// Every object of the cell, by its key, but the references of forks of its cells, which are
    /// never candidates, nor named by any state.
    pub objects: BTreeMap<String, Meta>,
    /// SHA-256 over every object listed but the collector's own (`<C>gc/`), in the order of their
    /// keys: each one's key, size, time written, and version (its ETag and its version, as the
    /// backend names them). Its lease, which every pass writes, and its marks, which no pass
    /// decides by, change it not; nor the read evidence of its versions, which passes trim skipping
    /// it ([`digest`]).
    pub digest: [u8; 32],
    /// Every object listed but the cell's collector lease, which recording counts as it writes it
    /// ([`record`]).
    pub usage: Usage,
    /// cladefs: the references of forks of the cell listed beside their marks
    /// ([`layout::retired`]): retired, or taken back, and written again since by a writer whose
    /// read of the child's record came before, garbage the pass deletes (TASK-395.13,
    /// TASK-395.14). Their writes changed the listing, so that no pass skips them.
    pub stale: Vec<String>,
    /// cladefs: how many references of forks of the cell it listed, the stale among
    /// them: none but those, a pass deleting them leaves none (TASK-455).
    pub references: usize,
}

/// Lists `cell`, once.
pub(crate) async fn list(bucket: &impl Bucket, cell: CellId) -> Result<Listed, bucket::Refused> {
    let measured = bucket.now();
    let mut listed = bucket::list(bucket, &cell.prefix()).await?;
    listed.sort_by(|a, b| a.key.cmp(&b.key));
    let lease = layout::collector_lease(cell);
    let digest = digest(cell, &listed);
    let mut usage = Usage { measured, ..Usage::default() };
    let mut objects = BTreeMap::new();
    let mut references = Vec::new();
    for meta in listed {
        if meta.key != lease {
            usage.add(&meta.key, meta.size);
        }
        match layout::kind(&meta.key) {
            // cladefs: a mirror's pin, and its hold, are references of its own (TASK-633.8).
            layout::Kind::Reference { .. }
            | layout::Kind::Pin { .. }
            | layout::Kind::Hold { .. } => references.push(meta.key),
            _ => drop(objects.insert(meta.key.clone(), meta)),
        }
    }
    let count = references.len();
    let stale = (references.into_iter())
        .filter(|key| layout::retired_of(key).is_some_and(|mark| objects.contains_key(&mark)))
        .collect();
    Ok(Listed { objects, digest, usage, stale, references: count })
}

/// The digest of a listing of `cell`, `listed`, in the order of their keys ([`Listed::digest`]):
/// of every object but the collector's lease (`<C>gc/lease`), which every pass writes, and the read
/// evidence of the cell's versions (`<C>reads/`, TASK-702.2), which forks' hosts write as often as
/// they open them, and every pass trims, skipping the cell or not ([`crate::evidence::doomed`]).
pub(crate) fn digest<'a>(cell: CellId, listed: impl IntoIterator<Item = &'a Meta>) -> [u8; 32] {
    let (lease, evidence) = (layout::collector_lease(cell), layout::evidence_of_cell(cell));
    let mut digest = Sha256::new();
    let decided = |meta: &&Meta| meta.key != lease && !meta.key.starts_with(&evidence);
    for meta in listed.into_iter().filter(decided) {
        digest_of(&mut digest, meta);
    }
    digest.finalize().into()
}

/// Feeds `meta` to `digest`, every part of it after its length, so that no two listings feed the
/// same bytes.
fn digest_of(digest: &mut Sha256, meta: &Meta) {
    let text = |digest: &mut Sha256, text: Option<&str>| match text {
        None => digest.update([0]),
        Some(text) => {
            digest.update([1]);
            digest.update((text.len() as u64).to_le_bytes());
            digest.update(text.as_bytes());
        }
    };
    text(digest, Some(&meta.key));
    digest.update(meta.size.to_le_bytes());
    digest.update(meta.modified.to_le_bytes());
    text(digest, meta.version.e_tag.as_deref());
    text(digest, meta.version.version.as_deref());
}

/// A pass's taking of its cell's collector lease ([`take`]).
pub(crate) struct LeaseTaking {
    /// The epoch it took.
    pub epoch: u64,
    /// Who took it.
    pub holder: u64,
    /// What the last pass over the cell that ran to its end recorded.
    pub passed: Option<Passed>,
}

/// The collector lease of `cell` as a read finds it, if there is one, for a taking of it after
/// ([`take`]).
pub(crate) async fn found(
    bucket: &impl Bucket,
    cell: CellId,
) -> Result<Option<bucket::Object>, bucket::Refused> {
    bucket::get(bucket, &layout::collector_lease(cell)).await
}

/// Takes the collector lease of `cell` as `holder`, holding it as `holding` says, at the epoch
/// after its last taking's, from `found`, what a read of it found ([`found`]): carrying what it
/// records to the next taking. Unless another holds it, unexpired ([`Left::Held`]), or a pass
/// that went through ended since the taker's sweep began ([`Left::Swept`]): then it leaves the
/// cell to that pass, writing nothing ([`Error::Left`]).
pub(crate) async fn take(
    bucket: &impl Bucket,
    cell: CellId,
    found: Option<bucket::Object>,
    holder: u64,
    holding: Holding,
) -> Result<LeaseTaking, Error> {
    let start = lifecycle::Start::Found(found);
    let decode = |bytes: &[u8]| CollectorLease::decode(cell, bytes);
    lifecycle::transition(bucket, &layout::collector_lease(cell), start, decode, |found| {
        // The time as it decides, after its read: a lease's expiry is never taken for later than
        // the read found it.
        let now = bucket.now();
        let (epoch, ended, passed) = match found {
            None => (1, 0, None),
            Some(lease) => match lease.leaves(holder, holding, now) {
                Some(left) => return Err(Error::Left(left)),
                None => (lease.epoch + 1, lease.ended, lease.passed),
            },
        };
        // Holding for no time, it holds nothing: a pass racing it, whose time was read before it
        // took the lease, does not find it held.
        let until = match holding.lease {
            0 => 0,
            lease => now.saturating_add(lease),
        };
        let lease = CollectorLease {
            format: FORMAT.current,
            epoch,
            holder,
            until,
            ended,
            passed: passed.clone(),
        };
        Ok(lifecycle::Step::Write(lease, LeaseTaking { epoch, holder, passed }))
    })
    .await
}

/// Changes the collector lease of `cell` as `change` says, if it is still `taken`: whether it
/// was. Its usage counts the lease as it is written.
pub(crate) async fn change(
    bucket: &impl Bucket,
    cell: CellId,
    taken: &LeaseTaking,
    change: impl Fn(CollectorLease) -> CollectorLease,
) -> Result<bool, Error> {
    let decode = |bytes: &[u8]| CollectorLease::decode(cell, bytes);
    let key = layout::collector_lease(cell);
    lifecycle::transition(bucket, &key, lifecycle::Start::Read, decode, |found| match found {
        Some(lease) if (lease.epoch, lease.holder) == (taken.epoch, taken.holder) => {
            Ok(lifecycle::Step::Write(change(lease).counting_itself(), true))
        }
        _ => Ok(lifecycle::Step::Stop(false)),
    })
    .await
}

/// The collector lease of `cell`, if there is one: [`Error::Format`] if of another format.
pub async fn read(bucket: &impl Bucket, cell: CellId) -> Result<Option<CollectorLease>, Error> {
    found(bucket, cell).await?.map(|object| CollectorLease::decode(cell, &object.bytes)).transpose()
}

/// The epoch of the last taking of the collector lease of `cell`: 0 if none took it.
pub(crate) async fn epoch(bucket: &impl Bucket, cell: CellId) -> Result<u64, Error> {
    Ok(read(bucket, cell).await?.map_or(0, |lease| lease.epoch))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bucket::Version;
    use crate::layout::TEST_CELL;

    /// Board 8467's review: a lease of a cell of another bucket, taken through a host's routed
    /// bucket, whose home bucket's backend is an hour ahead of that bucket's, ends by that bucket's
    /// clock, as its own hosts read it: taken by them once its backend's clock passed the lease's
    /// length, not an hour after. Its time is the least of the clocks of the buckets it reached.
    #[test]
    fn routed_leases_end_by_their_buckets_clock() {
        use crate::client::Client;
        use crate::object_bucket::ObjectBucket;
        use crate::object_bucket::tests::{runtime, s3};
        use crate::routed::{Resolve, RoutedBucket};
        use crate::s3_server::S3Server;
        use std::sync::Arc;
        struct Other(Arc<ObjectBucket>);
        impl Resolve for Other {
            fn open<'a>(
                &'a self,
                _: &'a ObjectBucket,
                _: u16,
            ) -> futures::future::BoxFuture<'a, Result<Arc<ObjectBucket>, Error>> {
                Box::pin(std::future::ready(Ok(self.0.clone())))
            }
        }
        let serving = runtime();
        let (ahead, behind) =
            serving.block_on(async { (S3Server::start(None), S3Server::start(None)) });
        ahead.clock_ahead(3_600_000);
        let home = s3(&ahead, "home", &Client::DEFAULT);
        let other = s3(&behind, "other", &Client::DEFAULT);
        let details = crate::identity::Details::default();
        serving.block_on(crate::identity::format(&*home, &details)).unwrap();
        let routed = RoutedBucket::new(home, Arc::new(Other(other.clone())));
        let cell = layout::test_cell(2 << 48 | 1);
        serving.block_on(lifecycle::create(&*other, cell, 0)).unwrap();
        let holding = Holding { lease: 60_000, since: None };
        let through = |holder| {
            serving.block_on(async {
                let lease = super::found(&routed, cell).await.unwrap();
                super::take(&routed, cell, lease, holder, holding).await.map(|taken| taken.epoch)
            })
        };
        let direct = |holder| {
            serving.block_on(async {
                let lease = super::found(&*other, cell).await.unwrap();
                super::take(&*other, cell, lease, holder, holding).await.map(|taken| taken.epoch)
            })
        };
        assert_eq!(through(1).unwrap(), 1);
        assert!(matches!(direct(2), Err(Error::Left(Left::Held))));
        behind.clock_ahead(62_000);
        assert_eq!(direct(2).unwrap(), 2);
    }

    /// Board 8471's AC #2: a lease whose holder's host is gone, its own clock an hour ahead of its
    /// bucket's backend, another host takes once the backend's clock passed the lease's length since
    /// its taking, and not before: as every host reads the backend's clock, not its own. By the
    /// hosts' clocks, which the backend's going on moves not, it stood held.
    #[test]
    fn leases_end_by_the_backends_clock() {
        use crate::client::Client;
        use crate::object_bucket::tests::{runtime, s3};
        use crate::s3_server::S3Server;
        let serving = runtime();
        let server = serving.block_on(async { S3Server::start(None) });
        let behind = -3_600_000;
        server.clock_ahead(behind);
        let (gone, other) =
            (s3(&server, "leases", &Client::DEFAULT), s3(&server, "leases", &Client::DEFAULT));
        let cell = layout::test_cell(1);
        serving.block_on(lifecycle::create(&*gone, cell, 0)).unwrap();
        let holding = Holding { lease: 60_000, since: None };
        // The epoch a taking by `holder` through `bucket` takes.
        let taking = |bucket: &crate::object_bucket::ObjectBucket, holder| {
            serving.block_on(async {
                let lease = super::found(bucket, cell).await.unwrap();
                super::take(bucket, cell, lease, holder, holding).await.map(|taken| taken.epoch)
            })
        };
        assert_eq!(taking(&gone, 1).unwrap(), 1);
        assert!(matches!(taking(&other, 2), Err(Error::Left(Left::Held))));
        server.clock_ahead(behind + 58_000);
        assert!(matches!(taking(&other, 2), Err(Error::Left(Left::Held))));
        server.clock_ahead(behind + 62_000);
        assert_eq!(taking(&other, 2).unwrap(), 2);
    }

    fn meta(key: &str, size: u64, modified: u64, e_tag: &str) -> Meta {
        let version = Version { e_tag: Some(e_tag.to_owned()), version: None };
        Meta { key: key.to_owned(), version, size, modified }
    }

    fn digest(metas: &[Meta]) -> [u8; 32] {
        let mut digest = Sha256::new();
        metas.iter().for_each(|meta| digest_of(&mut digest, meta));
        digest.finalize().into()
    }

    /// A listing's digest changes with any part of any object listed: its key, its size, when it
    /// was written, its version; and not with where one key ends and the next begins.
    #[test]
    fn digests_change_with_every_part_of_a_listing() {
        let record = layout::head(CellId { tenant: TEST_CELL.tenant, id: 1 });
        let base = [meta(&record, 10, 5, "1")];
        let changed = [
            vec![meta(&record, 11, 5, "1")],
            vec![meta(&record, 10, 6, "1")],
            vec![meta(&record, 10, 5, "2")],
            vec![Meta { version: Version::default(), ..meta(&record, 10, 5, "1") }],
            vec![meta(&format!("{record}0"), 10, 5, "1")],
            vec![meta(&record, 10, 5, "1"), meta(&format!("{record}0"), 10, 5, "1")],
            Vec::new(),
        ];
        for changed in changed {
            assert_ne!(digest(&changed), digest(&base), "{changed:?}");
        }
        assert_eq!(digest(&base), digest(&[meta(&record, 10, 5, "1")]));
        let joined = [meta("ab", 1, 1, "c"), meta("d", 1, 1, "e")];
        let split = [meta("a", 1, 1, "bc"), meta("d", 1, 1, "e")];
        assert_ne!(digest(&joined), digest(&split));
    }

    /// A lease's usage counts the lease as it is encoded, one object of its bytes.
    #[test]
    fn leases_count_themselves() {
        let mut usage = Usage { measured: 3, ..Usage::default() };
        usage.add(&layout::head(CellId { tenant: TEST_CELL.tenant, id: 1 }), 40);
        let passed = Some(Passed { listing: Some([7; 32]), usage });
        let lease = CollectorLease {
            format: FORMAT.current,
            epoch: 2,
            holder: 3,
            until: 4,
            ended: 5,
            passed,
        };
        let lease = lease.counting_itself();
        let len = layout::encode(&lease).len() as u64;
        let usage = &lease.passed.as_ref().unwrap().usage;
        assert_eq!(usage.kinds[GC_LEASE], Sum { objects: 1, bytes: len });
        assert_eq!(usage.total(), Sum { objects: 2, bytes: 40 + len });
        assert_eq!(CollectorLease::decode(TEST_CELL, &layout::encode(&lease)), Ok(lease));
    }

    /// A lease of another format is refused by name, before the rest of it is read.
    #[test]
    fn leases_of_other_formats_are_refused() {
        for format in [FORMAT.current + 1, FORMAT.oldest - 1, 8] {
            let lease =
                CollectorLease { format, epoch: 1, holder: 2, until: 0, ended: 0, passed: None };
            let refused = Error::Format { cell: TEST_CELL, format };
            assert_eq!(CollectorLease::decode(TEST_CELL, &layout::encode(&lease)), Err(refused));
        }
    }
}
