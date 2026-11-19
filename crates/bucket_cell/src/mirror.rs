// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: mirrors (backlog doc-23, Mirrors; TASK-633.8). A version of a cell forked into another
//! bucket is copied there, byte for byte at the same keys, so that the children there read it from
//! their own bucket, cheaply, and with the cell's own bucket out of reach. One mirror serves every
//! child of the version in that bucket.
//!
//! A mirror of `parent`'s version at `at` into this bucket, B, from the parent's, A, in the order
//! `specs/buckets` checks:
//!
//! 1. In B, its **pin** ([`layout::pin`]), a reference of its own under the parent's prefix, which
//!    keeps what it copies from B's collector while it copies ([`crate::collector::Collector::collect_copy`]).
//!    One mirror of a version holds it at a time, its bytes its own: another's standing, the
//!    mirror is under way, and this one does nothing ([`Mirrored::UnderWay`]). A pin unrefreshed
//!    for [`PIN_STALE`] is its mirror's no longer, its host dead: the next mirror takes it over.
//! 2. In A, its **hold** ([`layout::hold`]), a reference of its own under the parent's prefix
//!    there (`mirrorHolds`), before it reads the head: it keeps the version whole in A, and the
//!    parent's cell from being removed with what the version inherits from other cells, which the
//!    mirror does not copy, while the copy stands. B's collector deletes it as the copy goes.
//! 3. The parent's **head read in A**, naming the version, or the mirror is given up, its hold and
//!    pin dropped (`mirrorChecks`). A marker already in B goes straight to step 6
//!    (`mirrorResumesMarked`): read again, the head may have dropped the version since, its
//!    children left referring in A.
//! 4. **Every object of the version under the parent's prefix copied** into B, create-only: the
//!    checkpoint at the version, the objects its extents are in, along their moves, and the moves
//!    objects, read where the router sends them. An object needed gone from A gives the mirror
//!    up. Its pin refreshed as it goes, the mirror given up if it is no longer its own.
//! 5. In B, the create-only **marker** ([`layout::mirror`]), once every object is copied
//!    (`markerAfterCopy`), its pin refreshed right before, naming the epoch of the checkpoint it
//!    copied: from then on, reads of the version in B are served there
//!    ([`crate::object_bucket::ObjectBucket`]).
//! 6. **The children of B referring in A listed**, after the marker (`mirrorListsAfterMark`), and
//!    their references written in B, then retired in A.
//! 7. The pin dropped, if still its own.
//!
//! Every step is create-only, idempotent, or a deletion: a mirror stopped anywhere is begun again
//! from the start, its pin taken over once stale, what it wrote kept. B's collector begins it
//! again, finding its pin stale; the next fork of the version into B, and the next opening of a
//! child of it in B, do too.
//!
//! A fork into B refers in B once the marker is there ([`Here`], [`crate::lifecycle`]).

use crate::bucket::{self, Bucket, Created, Meta, Updated, Version};
use crate::layout::{self, CellId, Kind, Mirror, Reference};
use crate::moves::{Moved, MovesMaps};
use crate::recovery::{self, Checkpoints};
use crate::{Error, head, lifecycle};
use futures::future::BoxFuture;
use std::collections::BTreeSet;

/// What a mirror came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mirrored {
    /// Marked: the objects it copied (those there already counted too), and the children's
    /// references it moved from the parent's bucket to this one.
    Done { copied: usize, moved: usize },
    /// Given up, as the reason says: the version dropped, an object of it gone, or its pin taken
    /// over.
    GaveUp(String),
    /// Another mirror of the version holds its pin: under way, or its host dead not long enough
    /// ago to tell ([`PIN_STALE`]).
    UnderWay,
}

/// cladefs: how long, by its bucket's clock, a mirror's pin stands unrefreshed before it is held
/// stale: its mirror's host dead, or stopped as long. A stale pin keeps nothing from B's collector
/// ([`crate::collector::Collector::collect_copy`]), and the next mirror of its version takes it over.
/// A mirror refreshes its pin a tenth of that apart while it copies, and right before its marker:
/// a host stopped longer than this between the two, its pin deemed stale and the copy collected
/// meanwhile, the version dropped, would mark a copy not whole, as a lease's holder stopped as
/// long would write past its lease.
pub const PIN_STALE: u64 = 10 * 60 * 1000;

/// How long a mirror goes from one refresh of its pin to the next while it copies.
const PIN_REFRESH: u64 = PIN_STALE / 10;

/// Whether the pin `meta` lists in `own` is stale ([`PIN_STALE`]).
pub fn stale(own: &impl Bucket, meta: &Meta) -> bool {
    meta.modified.saturating_add(PIN_STALE) < own.now()
}

/// cladefs: a bucket as forks into it see the mirrors there of versions of other buckets' cells
/// (doc-23, Mirrors; TASK-633.8): [`Bucket::mirrors`]. A fork refers in this bucket once the
/// version's marker is there, and in the parent's before; having referred there, it reads the
/// marker again after the parent's head, and refers here if it is there then (`forksRecheck`,
/// `forksLeaveA`).
pub trait Here: Send + Sync {
    /// Whether the mirror of `parent`'s version at `at` is marked in this bucket, read there now:
    /// never for a cell of this bucket's own.
    fn marked(&self, parent: CellId, at: u64) -> BoxFuture<'_, bool>;

    /// Creates `bytes` at `key` in this bucket, whatever cell `key` is of ([`bucket::create`]).
    fn create<'a>(&'a self, key: &'a str, bytes: Vec<u8>) -> BoxFuture<'a, ()>;

    /// Learns the epochs of the checkpoints at the versions of `cell`, a cell of another bucket,
    /// whose mirrors here are marked, from their markers ([`crate::known::Known::named`]): a
    /// recovery through one of them then reads its checkpoint by key, here, `cell`'s head, in its
    /// own bucket, unread, which may be out of reach (doc-23, What the parent's bucket being out
    /// of reach costs). Nothing for a cell of this bucket's own.
    fn learn(&self, cell: CellId) -> BoxFuture<'_, ()>;
}

/// A mirror's pin, held: its bytes, its own, and the version of its last write.
struct Pin {
    key: String,
    bytes: Vec<u8>,
    version: Version,
    /// When it was last written, by its bucket's clock.
    written: u64,
}

impl Pin {
    /// The pin of the mirror of `parent`'s version at `at` in `own`, made, or taken over from a
    /// mirror whose pin is stale: none if another mirror's stands.
    async fn take(own: &impl Bucket, parent: CellId, at: u64) -> Option<Self> {
        let key = layout::pin(parent, at);
        let bytes = layout::encode(&Reference::of(rand::random()));
        let version = match bucket::create(own, &key, bytes.clone()).await {
            Created::Created(version) => version,
            Created::Exists(found) if stale(own, &found.meta) => {
                match bucket::update(own, &key, bytes.clone(), &found.meta.version).await {
                    Updated::Updated(version) => version,
                    Updated::Changed => return None,
                }
            }
            Created::Exists(_) => return None,
        };
        Some(Self { key, bytes, version, written: own.now() })
    }

    /// Writes it again, if [`PIN_REFRESH`] passed since it was last written, or `now`: false if it
    /// is no longer this mirror's, taken over or deleted as stale.
    async fn refresh(&mut self, own: &impl Bucket, now: bool) -> bool {
        if !now && own.now() < self.written.saturating_add(PIN_REFRESH) {
            return true;
        }
        match bucket::update(own, &self.key, self.bytes.clone(), &self.version).await {
            Updated::Updated(version) => {
                (self.version, self.written) = (version, own.now());
                true
            }
            Updated::Changed => false,
        }
    }

    /// Deletes it, if it is still this mirror's: written again first, conditionally, so that a
    /// mirror that took it over since keeps it.
    async fn release(mut self, own: &impl Bucket) {
        if self.refresh(own, true).await {
            bucket::delete(own, &self.key).await;
        }
    }
}

/// How many objects a mirror copies at once.
const COPIES_AT_ONCE: usize = 8;

/// Mirrors `parent`'s version at `at` into this bucket, whose own store is `own` and number
/// `number`, through `bucket`, routed, which reads the parent's bucket (see the module). Begun again
/// after a crash, it goes on from where it was, once its pin is stale.
pub async fn mirror(
    bucket: &impl Bucket,
    own: &impl Bucket,
    number: u16,
    parent: CellId,
    at: u64,
) -> Result<Mirrored, Error> {
    let Some(mut pin) = Pin::take(own, parent, at).await else {
        return Ok(Mirrored::UnderWay);
    };
    failpoint!("mirror::pinned");
    // Its hold in the parent's bucket, before it reads the head there (`mirrorHolds`).
    let hold = layout::hold(parent, at, number);
    bucket::create(bucket, &hold, layout::encode(&Reference::of(0))).await;
    failpoint!("mirror::held");
    let marker = layout::mirror(parent, at);
    let copied = match bucket::get(own, &marker).await {
        // Marked already, by a mirror begun before: its copy whole.
        Some(_) => 0,
        None => match copy(bucket, own, &mut pin, parent, at).await {
            Ok(Copied::Whole { count, epoch }) if pin.refresh(own, true).await => {
                bucket::create(own, &marker, layout::encode(&Mirror::of(epoch))).await;
                failpoint!("mirror::marked");
                count
            }
            // Its pin another's now, and its hold that mirror's too.
            Ok(Copied::Whole { .. } | Copied::Taken) => {
                return Ok(Mirrored::GaveUp(TAKEN.to_owned()));
            }
            Ok(Copied::Short(why)) => {
                let_go(bucket, own, pin, &hold).await;
                return Ok(Mirrored::GaveUp(why));
            }
            Err(error) => {
                let_go(bucket, own, pin, &hold).await;
                return Err(error);
            }
        },
    };
    let moved = refer_here(bucket, own, number, parent, at).await;
    failpoint!("mirror::referred");
    pin.release(own).await;
    Ok(Mirrored::Done { copied, moved })
}

/// Gives a mirror up: its hold deleted in the parent's bucket, then its pin let go.
async fn let_go(bucket: &impl Bucket, own: &impl Bucket, pin: Pin, hold: &str) {
    bucket::delete(bucket, hold).await;
    pin.release(own).await;
}

/// Why a mirror whose pin is no longer its own gives up.
const TAKEN: &str = "its pin, stale, taken over";

/// What a mirror's copy came to.
enum Copied {
    /// Every object copied, this many, of the version whose checkpoint is of `epoch`.
    Whole { count: usize, epoch: u64 },
    /// Its pin taken over, as it refreshed it.
    Taken,
    /// Short of an object, or of the version, as it says.
    Short(String),
}

/// Copies every object of `parent`'s version at `at` under its prefix into `own`, after reading
/// its head (`mirrorChecks`), refreshing `pin` as it goes.
///
/// What it must copy: the checkpoint at the version and what recovering it read, and each extent's
/// object where its moves lead, with every moves object on the way. What it copies if it is there:
/// where the extent was before its moves, which the parent's bucket's collector keeps for the
/// version (doc-7, A pass over a cell) unless it compacted it away before the version was cut,
/// its state naming the extent where it was sealed still; readers follow its moves, copied too.
async fn copy(
    bucket: &impl Bucket,
    own: &impl Bucket,
    pin: &mut Pin,
    parent: CellId,
    at: u64,
) -> Result<Copied, Error> {
    let head = head::get(bucket, parent).await?;
    let named = head.as_ref().filter(|head| !head.deleted()).and_then(|head| {
        head.versions.iter().any(|version| version.at == at).then(|| head.epoch_of(at)).flatten()
    });
    let Some(epoch) = named else {
        return Ok(Copied::Short(format!("{parent}'s head names no version at {at:x}")));
    };
    failpoint!("mirror::checked");
    let checkpoints = Checkpoints::new(bucket, 1);
    let (state, touched) = match recovery::recover_at(&checkpoints, parent, at).await {
        Ok(recovered) => recovered,
        Err(Error::Lost(lost)) => {
            return Ok(Copied::Short(format!("{lost}: gone from its bucket")));
        }
        Err(error) => return Err(error),
    };
    let ours = |key: &String| layout::owner(key) == Some(parent);
    let mut needed: BTreeSet<String> = touched.into_iter().filter(ours).collect();
    needed.insert(layout::checkpoint(parent, at, epoch));
    let mut before = BTreeSet::new();
    let mut maps = MovesMaps::default();
    for (&extent, location) in state.extents.iter().filter(|(_, location)| ours(&location.key)) {
        let mut along = location.clone();
        while let Moved::To(to) = maps.moved(bucket, extent, &along.key).await? {
            before.insert(std::mem::replace(&mut along, to).key);
        }
        needed.insert(along.key);
    }
    let mut moves = Vec::new();
    for key in needed.iter().chain(&before) {
        let prefix = layout::moves_of(key)?;
        moves.extend(bucket::list(bucket, &prefix).await.into_iter().map(|meta| meta.key));
    }
    needed.extend(moves);
    before.retain(|key| !needed.contains(key));
    let keys: Vec<(String, bool)> = (needed.into_iter().map(|key| (key, true)))
        .chain(before.into_iter().map(|key| (key, false)))
        .collect();
    use futures::StreamExt as _;
    let mut count = 0;
    for chunk in keys.chunks(COPIES_AT_ONCE * 4) {
        if !pin.refresh(own, false).await {
            return Ok(Copied::Taken);
        }
        let copies: Vec<_> = chunk.iter().map(|(key, _)| copy_one(bucket, own, key)).collect();
        let copied: Vec<bool> =
            futures::stream::iter(copies).buffer_unordered(COPIES_AT_ONCE).collect().await;
        let short = chunk.iter().zip(&copied).find(|((_, needed), copied)| *needed && !**copied);
        if let Some(((gone, _), _)) = short {
            return Ok(Copied::Short(format!("{gone}: gone from its bucket")));
        }
        count += copied.iter().filter(|copied| **copied).count();
    }
    failpoint!("mirror::copied");
    Ok(Copied::Whole { count, epoch })
}

/// Copies the object at `key` into `own`, unless it is there: false if it is gone from where
/// `bucket` reads it. One there already, from an earlier copy, B's collector keeps while the
/// parent's cell lives ([`crate::collector::Collector::collect_copy`]): no version of it this
/// mirror copies outlives it.
async fn copy_one(bucket: &impl Bucket, own: &impl Bucket, key: &str) -> bool {
    if bucket::get(own, key).await.is_some() {
        return true;
    }
    let Some(object) = bucket::get(bucket, key).await else { return false };
    bucket::create(own, key, object.bytes).await;
    // A crash with some objects copied, others not (a test's).
    fail::fail_point!("mirror::copying");
    true
}

/// Moves the references of the children of this bucket, numbered `number`, to `parent`'s version
/// at `at` from the parent's bucket to `own` (`mirrorListsAfterMark`): each written here, then
/// retired there. How many.
async fn refer_here(
    bucket: &impl Bucket,
    own: &impl Bucket,
    number: u16,
    parent: CellId,
    at: u64,
) -> usize {
    let listed = bucket::list(bucket, &layout::origin(parent, at)).await;
    let mut moved = Vec::new();
    for meta in listed {
        let Kind::Reference { child, .. } = layout::kind(&meta.key) else { continue };
        if crate::objectid::bucket_of(child.id) != number {
            continue;
        }
        let Some(object) = bucket::get(bucket, &meta.key).await else { continue };
        bucket::create(own, &meta.key, object.bytes).await;
        moved.push(meta.key);
    }
    // A crash with the references written here, still standing there (a test's).
    fail::fail_point!("mirror::moved");
    let keys: Vec<&str> = moved.iter().map(String::as_str).collect();
    lifecycle::retire(bucket, &keys).await;
    moved.len()
}
