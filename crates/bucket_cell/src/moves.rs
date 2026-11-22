// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The collector's moves (doc-7, Layout): where the live extents of an object it compacted went,
//! which a reader that finds the object gone follows, a writer records in its next checkpoint, and
//! the collector keeps while a cell's way to its extents goes through them.

use crate::bucket::{self, Bucket};
use crate::layout::{self, Location, Moves};
use crate::{Error, object};
use futures::{StreamExt, TryStreamExt};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use storage_device::ExtentId;

/// How many times an extent may have moved.
pub(crate) const MOVES: usize = 64;

/// Whether the collector moved an extent when it compacted the object holding it.
pub enum Moved {
    /// It did not compact the object.
    No,
    To(Location),
    /// It compacted the object and left the extent out: no live cell held it.
    Dropped,
}

/// The maps of compacted objects' moves, each read once.
#[derive(Default)]
pub struct MovesMaps {
    maps: HashMap<String, Option<BTreeMap<ExtentId, Location>>>,
}

/// How many objects' moves [`MovesMaps::read`] reads at once: each a round trip to object storage.
const READ_AT_ONCE: usize = 32;

impl MovesMaps {
    /// Reads the moves of the objects at `keys` not read yet: a checkpoint follows every extent of
    /// its cell, each object's a round trip, which one at a time, as the cell grew, held the
    /// uploader up for tens of seconds at 25 ms of latency (TASK-91.2). Few objects have moves:
    /// a listing of each of the keys' prefixes' says which, whose moves are read, several at once.
    /// A collector's moves written since are missed, as a read of them before they were would.
    pub async fn read(
        &mut self,
        bucket: &impl Bucket,
        keys: impl IntoIterator<Item = String>,
    ) -> Result<(), Error> {
        let unread: BTreeSet<String> =
            keys.into_iter().filter(|key| !self.maps.contains_key(key)).collect();
        let prefixes: BTreeSet<&str> =
            unread.iter().map(|key| &key[..key.rfind('/').map_or(0, |slash| slash + 1)]).collect();
        let mut moved: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for prefix in prefixes {
            for meta in bucket::list(bucket, &layout::moves_under(prefix)?).await? {
                if let Some(from) = layout::moved_from(&meta.key) {
                    moved.entry(from).or_default().push(meta.key);
                }
            }
        }
        let (unread, unmoved): (Vec<String>, Vec<String>) =
            unread.into_iter().partition(|key| moved.contains_key(key));
        self.maps.extend(unmoved.into_iter().map(|key| (key, None)));
        let unread = unread.into_iter().map(|key| {
            let moves = moved.remove(&key).unwrap_or_default();
            (key, moves)
        });
        self.read_listed(bucket, unread).await
    }

    /// cladefs: reads the moves of the objects at the keys `listed` gives, each with the keys of its
    /// moves a listing found, in the order of their keys, which is their passes' (TASK-407.1), the
    /// objects not read yet, [`READ_AT_ONCE`] at once (TASK-395.5): the collector's, which listed
    /// every object before. Moves gone since read as none, as they would one at a time.
    pub(crate) async fn read_listed(
        &mut self,
        bucket: &impl Bucket,
        listed: impl IntoIterator<Item = (String, Vec<String>)>,
    ) -> Result<(), Error> {
        let unread: BTreeMap<String, Vec<String>> =
            listed.into_iter().filter(|(key, _)| !self.maps.contains_key(key)).collect();
        let read: Vec<_> = futures::stream::iter(unread)
            .map(|(key, moves)| async move { Ok::<_, Error>((key, merged(bucket, &moves).await?)) })
            .buffer_unordered(READ_AT_ONCE)
            .try_collect()
            .await?;
        self.maps.extend(read);
        Ok(())
    }

    /// cladefs: whether the collector moved `extent` from the object at `key`, and where to, of
    /// moves read before ([`MovesMaps::read_listed`]).
    pub(crate) fn moved_read(&self, extent: ExtentId, key: &str) -> Moved {
        match self.maps.get(key).unwrap_or_else(|| panic!("{key}: its moves not read")) {
            None => Moved::No,
            Some(map) => map.get(&extent).cloned().map_or(Moved::Dropped, Moved::To),
        }
    }

    /// Whether the collector moved `extent` from the object at `key`, and where to.
    pub async fn moved(
        &mut self,
        bucket: &impl Bucket,
        extent: ExtentId,
        key: &str,
    ) -> Result<Moved, Error> {
        if !self.maps.contains_key(key) {
            let listed = bucket::list(bucket, &layout::moves_of(key)?).await?;
            let moves: Vec<String> = listed.into_iter().map(|meta| meta.key).collect();
            let map = merged(bucket, &moves).await?;
            self.maps.insert(key.to_owned(), map);
        }
        Ok(self.moved_read(extent, key))
    }

    /// Where `extent` at `location` is now, following its moves as far as they go.
    pub async fn follow(
        &mut self,
        bucket: &impl Bucket,
        extent: ExtentId,
        location: &Location,
    ) -> Result<Location, Error> {
        let mut location = location.clone();
        // An extent the collector dropped no live cell held.
        while let Moved::To(to) = self.moved(bucket, extent, &location.key).await? {
            location = to;
        }
        Ok(location)
    }
}

/// cladefs: where the moves at `keys`, of one object, each a pass's, in the order of their passes,
/// moved its extents (TASK-407.1): each extent where the latest that has it put it, which every
/// reader and the collector follow alike; none if none of them is there. One pass's alone, but for
/// a pass slow to create them, whose moves of the extents it found live are a copy of them as
/// well, or of extents later passes found dead.
async fn merged(
    bucket: &impl Bucket,
    keys: &[String],
) -> Result<Option<BTreeMap<ExtentId, Location>>, Error> {
    let mut map: Option<BTreeMap<ExtentId, Location>> = None;
    for key in keys {
        if let Some(moves) = object::footer::<Moves>(bucket, key).await? {
            map.get_or_insert_default().extend(moves.extents);
        }
    }
    Ok(map)
}
