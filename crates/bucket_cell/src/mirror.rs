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
//! 2. The parent's **head read in A**, naming the version, or the mirror is given up, its pin
//!    dropped (`mirrorChecks`). A marker already in B goes straight to step 5
//!    (`mirrorResumesMarked`): read again, the head may have dropped the version since, its
//!    children left referring in A.
//! 3. **Every object of the version under the parent's prefix copied** into B, create-only: the
//!    checkpoint at the version, the segments and extents' objects it names, and their moves
//!    objects, read where the router sends them. An object gone from A gives the mirror up. Objects
//!    of other cells the version inherited are not copied: they are read where they are.
//! 4. In B, the create-only **marker** ([`layout::mirror`]), once every object is copied
//!    (`markerAfterCopy`), naming the epoch of the checkpoint it copied: from then on, reads of the
//!    version in B are served there ([`crate::object_bucket::ObjectBucket`]).
//! 5. **The children of B referring in A listed**, after the marker (`mirrorListsAfterMark`), and
//!    their references written in B, then retired in A.
//! 6. The pin dropped.
//!
//! Every step is create-only, idempotent, or a deletion: a mirror stopped anywhere is begun again
//! from the start, its pin found, what it wrote kept.

use crate::bucket::{self, Bucket, Created};
use crate::layout::{self, CellId, Kind, Mirror, Reference};
use crate::recovery::{self, Checkpoints};
use crate::{Error, head, lifecycle};
use std::collections::BTreeSet;

/// What a mirror came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mirrored {
    /// Marked: the objects it copied (those there already counted too), and the children's
    /// references it moved from the parent's bucket to this one.
    Done { copied: usize, moved: usize },
    /// Given up, as the reason says: the version dropped, or an object of it gone.
    GaveUp(String),
}

/// How many objects a mirror copies at once.
const COPIES_AT_ONCE: usize = 8;

/// Mirrors `parent`'s version at `at` into this bucket, whose own store is `own` and number
/// `number`, through `bucket`, routed, which reads the parent's bucket (see the module). Begun again
/// after a crash, it goes on from where it was.
pub async fn mirror(
    bucket: &impl Bucket,
    own: &impl Bucket,
    number: u16,
    parent: CellId,
    at: u64,
) -> Result<Mirrored, Error> {
    let pin = layout::pin(parent, at);
    let pinned = layout::encode(&Reference::of(0));
    if let Created::Exists(_) = bucket::create(own, &pin, pinned).await {
        // Another write at the pin's key than a pin: none of a mirror's.
        return Err(Error::Corrupt(format!("{pin}: not a mirror's pin")));
    }
    let marker = layout::mirror(parent, at);
    let copied = match bucket::get(own, &marker).await {
        // Marked already, by this mirror begun before: its copy whole.
        Some(_) => 0,
        None => match copy(bucket, own, parent, at).await? {
            Ok((copied, epoch)) => {
                bucket::create(own, &marker, layout::encode(&Mirror::of(epoch))).await;
                copied
            }
            Err(why) => {
                bucket::delete(own, &pin).await;
                return Ok(Mirrored::GaveUp(why));
            }
        },
    };
    let moved = refer_here(bucket, own, number, parent, at).await;
    bucket::delete(own, &pin).await;
    Ok(Mirrored::Done { copied, moved })
}

/// Copies every object of `parent`'s version at `at` under its prefix into `own`, after reading
/// its head (`mirrorChecks`): how many, and the epoch of the checkpoint at the version; or why the
/// mirror is given up.
async fn copy(
    bucket: &impl Bucket,
    own: &impl Bucket,
    parent: CellId,
    at: u64,
) -> Result<Result<(usize, u64), String>, Error> {
    let head = head::get(bucket, parent).await?;
    let named = head.as_ref().filter(|head| !head.deleted()).and_then(|head| {
        head.versions.iter().any(|version| version.at == at).then(|| head.epoch_of(at)).flatten()
    });
    let Some(epoch) = named else {
        return Ok(Err(format!("{parent}'s head names no version at {at:x}")));
    };
    let checkpoints = Checkpoints::new(bucket, 1);
    let (state, touched) = match recovery::recover_at(&checkpoints, parent, at).await {
        Ok(recovered) => recovered,
        Err(Error::Lost(lost)) => return Ok(Err(format!("{lost}: gone from its bucket"))),
        Err(error) => return Err(error),
    };
    // The checkpoint, the objects the state names and those its recovery read, of the parent's own.
    let mut keys: BTreeSet<String> = touched;
    keys.insert(layout::checkpoint(parent, at, epoch));
    keys.extend(state.extents.values().map(|location| location.key.clone()));
    keys.retain(|key| layout::owner(key) == Some(parent));
    // And their moves, and the moves of those.
    let mut moves = Vec::new();
    for key in &keys {
        if matches!(layout::kind(key), Kind::Moves { .. }) {
            continue;
        }
        let prefix = layout::moves_of(key)?;
        moves.extend(bucket::list(bucket, &prefix).await.into_iter().map(|meta| meta.key));
    }
    keys.extend(moves);
    let keys: Vec<String> = keys.into_iter().collect();
    let copies: Vec<_> = keys.iter().map(|key| copy_one(bucket, own, key)).collect();
    use futures::StreamExt as _;
    let copied: Vec<bool> =
        futures::stream::iter(copies).buffer_unordered(COPIES_AT_ONCE).collect().await;
    if let Some(gone) = keys.iter().zip(&copied).find(|(_, copied)| !**copied) {
        return Ok(Err(format!("{}: gone from its bucket", gone.0)));
    }
    Ok(Ok((keys.len(), epoch)))
}

/// Copies the object at `key` into `own`, unless it is there: false if it is gone from where
/// `bucket` reads it.
async fn copy_one(bucket: &impl Bucket, own: &impl Bucket, key: &str) -> bool {
    if bucket::get(own, key).await.is_some() {
        return true;
    }
    let Some(object) = bucket::get(bucket, key).await else { return false };
    bucket::create(own, key, object.bytes).await;
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
    let keys: Vec<&str> = moved.iter().map(String::as_str).collect();
    lifecycle::retire(bucket, &keys).await;
    moved.len()
}
