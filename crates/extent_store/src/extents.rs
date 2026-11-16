// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a branch's extents, as its store holds them, in two layers (TASK-453). Under, the
//! bucket's: where its recovered state, as the store opened, places what the bucket held of the
//! branch, a persistent map (`extent_remote::recovery::Extents`) shared, never changed here, with the
//! branch's writer and with every other store open on the host that started from the same decoded
//! state, as a base's forks do (`extent_remote::recovery::Decoded`). Over it, the store's own:
//! what it wrote, sealed, acknowledged or followed where the collector moved it since, and the
//! extents of the bucket's layer it holds no more there, freed or overlaid. A lookup reads its own
//! layer, then the bucket's. So a store's memory grows with what it does, not with what it
//! inherited: each fork's copy of its base's map in a store's own was 220-470 bytes an extent of
//! the base (TASK-443.2), hundreds of forks of a base of hundreds of thousands of extents tens of
//! gigabytes.
//!
//! What every store holds of its own, its own layer and a bucket's layer that started from no
//! decoded state, which it shares with its writer alone, is counted for the process
//! ([`own_entries`]), which the extent daemon's budget of its anonymous memory counts, as it counts
//! the decoded states the bucket's layers share once each, however many stores share them
//! (`extent_remote::recovery::Decoded::held`).

use crate::layout::{Data, Extent};
use extent_frame::segment::Location;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use storage_device::ExtentId;

/// A branch's extents, in layers. See the module's docs.
pub(crate) struct Extents {
    /// The bucket's layer: where the branch's state as the store opened placed its extents.
    bucket: extent_remote::recovery::Extents,
    /// The decoded state the bucket's layer shares, if it started from one: held, so that the
    /// stores opened after this one find it decoded, and share it too, while this one is open.
    layer: Option<Arc<extent_remote::State>>,
    /// The store's own layer: every extent it holds but those the bucket's holds still.
    own: HashMap<ExtentId, Extent>,
    /// Those of the bucket's layer it holds no more there: freed, or in its own layer.
    hidden: HashSet<ExtentId>,
}

/// The entries the stores of this process hold of their own ([`own_entries`]).
static OWN_ENTRIES: AtomicU64 = AtomicU64::new(0);

/// The entries the stores open in this process hold of their own: of their own layers, those they
/// hide of their bucket's, and the bucket's layers that started from no decoded state, which no
/// other store shares. What the extent daemon counts of what they hold of their own
/// ([`OWN_ENTRY_BYTES`]); the decoded states the others share it counts once each.
pub fn own_entries() -> u64 {
    OWN_ENTRIES.load(Ordering::Relaxed)
}

/// The most bytes of the process's heap an entry of a store's own layer holds, its writer's map's
/// entry for it once acknowledged included: what the extent daemon counts each of
/// [`own_entries`] as. A bound, measured by `bench_fork_extent_memory` (cladefs, TASK-453), which
/// fails past it: 613 bytes a block written, committed and uploaded, with what else the store and
/// its writer keep of it, its location's key a string of its own in each map (TASK-472).
pub const OWN_ENTRY_BYTES: u64 = 1024;

impl Default for Extents {
    fn default() -> Self {
        Self::over(extent_remote::recovery::Extents::new(), None)
    }
}

impl Drop for Extents {
    fn drop(&mut self) {
        OWN_ENTRIES.fetch_sub(self.weight(), Ordering::Relaxed);
    }
}

impl Extents {
    /// The extents `bucket` places, the bucket's layer, sharing `layer`, the decoded state it
    /// started from, if it did; none of the store's own.
    pub fn over(
        bucket: extent_remote::recovery::Extents,
        layer: Option<Arc<extent_remote::State>>,
    ) -> Self {
        let extents = Self { bucket, layer, own: HashMap::new(), hidden: HashSet::new() };
        OWN_ENTRIES.fetch_add(extents.weight(), Ordering::Relaxed);
        extents
    }

    /// The entries it holds of its own ([`own_entries`]): of its own layer, those it hides of the
    /// bucket's, and the bucket's layer's, unless it shares a decoded state.
    fn weight(&self) -> u64 {
        let unshared = if self.layer.is_none() { self.bucket.len() } else { 0 };
        (self.own.len() + self.hidden.len() + unshared) as u64
    }

    /// Runs `change` on it, counting what it changes of its weight for the process.
    fn changing<R>(&mut self, change: impl FnOnce(&mut Self) -> R) -> R {
        let before = self.weight();
        let changed = change(self);
        let after = self.weight();
        if after > before {
            OWN_ENTRIES.fetch_add(after - before, Ordering::Relaxed);
        } else if after < before {
            OWN_ENTRIES.fetch_sub(before - after, Ordering::Relaxed);
        }
        changed
    }

    /// How many it holds.
    #[cfg(any(test, feature = "audit"))]
    pub fn len(&self) -> usize {
        self.own.len() + self.bucket.len() - self.hidden.len()
    }

    /// Where the bucket's layer places `extent`, if it holds it there still.
    fn in_bucket(&self, extent: &ExtentId) -> Option<&Location> {
        let location = self.bucket.get(extent)?;
        (!self.hidden.contains(extent)).then_some(location)
    }

    /// `extent`'s entry, if it holds it: its own, or made of where the bucket's layer places it.
    pub fn get(&self, extent: &ExtentId) -> Option<Cow<'_, Extent>> {
        match self.own.get(extent) {
            Some(entry) => Some(Cow::Borrowed(entry)),
            None => self.in_bucket(extent).map(|location| Cow::Owned(remote(location))),
        }
    }

    /// `extent`'s entry in its own layer, to change, if it is there: one the store wrote, sealed,
    /// acknowledged or followed, which the bucket's layer never is.
    pub fn own_mut(&mut self, extent: &ExtentId) -> Option<&mut Extent> {
        self.own.get_mut(extent)
    }

    /// `extent`'s entry, which a write to it changes: its own, or `make`'s, entered, if it holds
    /// none. An extent written is open, or new, the branch's: never in the bucket's layer.
    pub fn written(&mut self, extent: ExtentId, make: impl FnOnce() -> Extent) -> &mut Extent {
        debug_assert!(self.in_bucket(&extent).is_none(), "{extent:?}, written, is the bucket's");
        if !self.own.contains_key(&extent) {
            OWN_ENTRIES.fetch_add(1, Ordering::Relaxed);
        }
        self.own.entry(extent).or_insert_with(make)
    }

    /// Holds `extent` at `entry`, in its own layer, over the bucket's.
    pub fn insert(&mut self, extent: ExtentId, entry: Extent) {
        self.changing(|extents| {
            if extents.bucket.contains_key(&extent) {
                extents.hidden.insert(extent);
            }
            extents.own.insert(extent, entry);
        });
    }

    /// Holds `extent` no more: returns its entry, if it held it.
    pub fn remove(&mut self, extent: &ExtentId) -> Option<Extent> {
        self.changing(|extents| {
            if let Some(entry) = extents.own.remove(extent) {
                return Some(entry);
            }
            let entry = remote(extents.in_bucket(extent)?);
            extents.hidden.insert(*extent);
            Some(entry)
        })
    }

    /// Names `extent`, if it holds it in the bucket, at `to`, where the collector moved it, if it
    /// names it on its way there ([`Data::follow`]): in its own layer, if the bucket's places it.
    pub fn follow(&mut self, extent: &ExtentId, to: &Location) {
        if let Some(entry) = self.own.get_mut(extent) {
            entry.data.follow(to);
            return;
        }
        let Some(at) = self.in_bucket(extent) else { return };
        if extent_remote::layout::further(&at.key, &to.key) {
            let len = at.len;
            self.insert(*extent, Extent { len, data: Data::Remote(to.clone()) });
        }
    }

    /// Its extents, with their entries, in no order.
    pub fn iter(&self) -> impl Iterator<Item = (ExtentId, Cow<'_, Extent>)> {
        let own = self.own.iter().map(|(extent, entry)| (*extent, Cow::Borrowed(entry)));
        own.chain(self.bucket_entries().map(|(extent, at)| (extent, Cow::Owned(remote(at)))))
    }

    /// Its extents, with their lengths, in no order: making no entry of the bucket's layer.
    pub fn lens(&self) -> impl Iterator<Item = (ExtentId, u64)> {
        let own = self.own.iter().map(|(extent, entry)| (*extent, entry.len));
        own.chain(self.bucket_entries().map(|(extent, at)| (extent, at.len)))
    }

    /// The entries of the bucket's layer it holds there still.
    fn bucket_entries(&self) -> impl Iterator<Item = (ExtentId, &Location)> {
        (self.bucket.iter())
            .filter(|(extent, _)| !self.hidden.contains(extent))
            .map(|(extent, at)| (*extent, at))
    }

    /// Its own layer's entries: every extent whose data is here, in segments or files of its own,
    /// is among them; the bucket's layer's is in the bucket.
    pub fn own(&self) -> impl Iterator<Item = (&ExtentId, &Extent)> {
        self.own.iter()
    }

    /// cladefs: whether it shares its bucket's layer with `other`'s: one map, not two alike.
    #[cfg(any(test, feature = "testing"))]
    pub fn shares_bucket_with(&self, other: &Self) -> bool {
        self.bucket.ptr_eq(&other.bucket)
    }

    /// cladefs: the decoded state its bucket's layer shares, if any.
    #[cfg(any(test, feature = "testing"))]
    pub fn layer(&self) -> Option<&Arc<extent_remote::State>> {
        self.layer.as_ref()
    }
}

/// The entry of an extent the bucket holds at `location`.
fn remote(location: &Location) -> Extent {
    Extent { len: location.len, data: Data::Remote(location.clone()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Piece, SegmentId};

    /// Where the bucket holds an extent: in a segment of the base's, at `offset`.
    fn at(offset: u64) -> Location {
        let key = extent_remote::layout::segment(extent_remote::layout::test_branch(1), 1, 1);
        Location { key, offset, len: 4096 }
    }

    fn extent(sequence: u64) -> ExtentId {
        ExtentId::new(1, sequence)
    }

    fn pieces(number: u64) -> Extent {
        let piece =
            Piece { segment: SegmentId { branch: 2, number }, offset: 0, extent_offset: 0, len: 8 };
        Extent { len: 8, data: Data::Pieces(vec![piece]) }
    }

    /// The bucket's layer, as the store opened: extents 0 to 9, in the bucket.
    fn bucket() -> extent_remote::recovery::Extents {
        (0..10).map(|sequence| (extent(sequence), at(sequence * 10))).collect()
    }

    fn held(extents: &Extents) -> Vec<(ExtentId, Extent)> {
        let mut held: Vec<_> =
            extents.iter().map(|(extent, entry)| (extent, entry.into_owned())).collect();
        held.sort_by_key(|(extent, _)| *extent);
        held
    }

    /// A store's extents read through its own layer to the bucket's; what it writes, frees,
    /// overlays and follows is its own, the bucket's layer unchanged, shared with another store
    /// opened from the same decoded state, which holds it all still; and the process counts what
    /// each holds of its own, until it is dropped: a bucket's layer that shares no decoded state
    /// among it.
    #[test]
    fn stores_layer_their_own_over_the_buckets() {
        let base = bucket();
        let decoded = extent_remote::State::of(extent_remote::layout::test_branch(1), base.clone());
        let layer = Some(Arc::new(decoded));
        let counted = own_entries();
        let (mut one, other) =
            (Extents::over(base.clone(), layer.clone()), Extents::over(base.clone(), layer));
        assert_eq!(own_entries(), counted);
        assert!(one.shares_bucket_with(&other));
        assert_eq!(one.len(), 10);
        assert_eq!(one.get(&extent(3)).as_deref(), Some(&remote(&at(30))));

        // Its own: written, freed, overlaid, followed.
        one.written(extent(20), || pieces(1)).len = 8;
        assert_eq!(one.remove(&extent(4)), Some(remote(&at(40))));
        assert_eq!(one.remove(&extent(4)), None);
        one.insert(extent(5), pieces(2));
        let key = extent_remote::layout::moves(&at(60).key, 1).unwrap();
        let moved = Location { key, offset: 0, len: 4096 };
        one.follow(&extent(6), &moved);
        let elsewhere = extent_remote::layout::segment(extent_remote::layout::test_branch(1), 2, 1);
        let unrelated = Location { key: elsewhere, offset: 0, len: 4096 };
        one.follow(&extent(7), &unrelated);
        one.follow(&extent(4), &moved);
        assert_eq!(one.remove(&extent(20)), Some(pieces(1)));

        assert_eq!(one.len(), 9);
        assert!(one.get(&extent(4)).is_none() && one.get(&extent(20)).is_none());
        assert_eq!(one.get(&extent(5)).as_deref(), Some(&pieces(2)));
        assert_eq!(one.get(&extent(6)).unwrap().data, Data::Remote(moved.clone()));
        assert_eq!(one.get(&extent(7)).unwrap().data, Data::Remote(at(70)));
        let mut expected: Vec<_> =
            base.iter().map(|(extent, location)| (*extent, remote(location))).collect();
        expected.retain(|(held, _)| *held != extent(4));
        expected[4].1 = pieces(2);
        expected[5].1 = Extent { len: 4096, data: Data::Remote(moved) };
        assert_eq!(held(&one), expected);
        let lens: Vec<_> = {
            let mut lens: Vec<_> = one.lens().collect();
            lens.sort();
            lens
        };
        let entries: Vec<_> = expected.iter().map(|(extent, entry)| (*extent, entry.len)).collect();
        assert_eq!(lens, entries);
        assert_eq!(one.own().count(), 2);
        // Its own: 5 and 6; hidden: 4, 5 and 6.
        assert_eq!(own_entries() - counted, 5);

        // The other's, and the bucket's layer, unchanged.
        assert!(one.shares_bucket_with(&other));
        let whole: Vec<_> =
            base.iter().map(|(extent, location)| (*extent, remote(location))).collect();
        assert_eq!(held(&other), whole);
        drop(one);
        assert_eq!(own_entries(), counted);
        let alone = Extents::over(base, None);
        assert_eq!(own_entries() - counted, 10);
        drop(alone);
        assert_eq!(own_entries(), counted);
    }
}
