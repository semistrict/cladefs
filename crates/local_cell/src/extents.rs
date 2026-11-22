// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a cell's extents, as its cell holds them, in two layers (TASK-453). Under, the
//! bucket's: where its recovered state, as the cell opened, places what the bucket held of the
//! cell, a persistent map (`bucket_cell::recovery::Extents`) shared, never changed here, with the
//! cell's writer and with every other cell open on the host that started from the same decoded
//! state, as a base's forks do (`bucket_cell::recovery::Decoded`). Over it, the cell's own:
//! what it wrote, encrypted, acknowledged or followed where the collector moved it since, and the
//! extents of the bucket's layer it holds no more there, freed or overlaid. A lookup reads its own
//! layer, then the bucket's. So a cell's memory grows with what it does, not with what it
//! inherited: each fork's copy of its base's map in a cell's own was 220-470 bytes an extent of
//! the base (TASK-443.2), hundreds of forks of a base of hundreds of thousands of extents tens of
//! gigabytes.
//!
//! What every cell holds of its own, its own layer and a bucket's layer that started from no
//! decoded state, which it shares with its writer alone, and what its writer's map, the bucket's
//! layer's shared, copies of it as it changes it, is counted for the process ([`own_held`]), which
//! the extent daemon's budget of its anonymous memory counts, as it counts the decoded states the
//! bucket's layers share once each, however many cells share them
//! (`bucket_cell::recovery::Decoded::held`).
//!
//! cladefs: and how dense the cell's own segments in the bucket are ([`Density`], doc-27): the
//! bytes of the extents it holds in each, and each one's data, which its writer relocates out of
//! once they are sparse.

use crate::layout::{Data, Extent};
use bucket_cell::recovery::{ENTRY_BYTES, RUN_BYTES};
use extent_frame::segment::CellId;
use extent_frame::segment::Location;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use storage_device::ExtentId;

/// A cell's extents, in layers. See the module's docs.
pub(crate) struct Extents {
    /// The bucket's layer: where the cell's state as the cell opened placed its extents.
    bucket: bucket_cell::recovery::Extents,
    /// The decoded state the bucket's layer shares, if it started from one: held, so that the
    /// cells opened after this one find it decoded, and share it too, while this one is open.
    layer: Option<Arc<bucket_cell::State>>,
    /// The cell's own layer: every extent it holds but those the bucket's holds still.
    own: HashMap<ExtentId, Extent>,
    /// Those of the bucket's layer it holds no more there: freed, or in its own layer.
    hidden: HashSet<ExtentId>,
    /// The runs those make in the bucket's layer's order, side by side, none it holds there
    /// between those of a run.
    hidden_runs: usize,
    /// cladefs: the cell's, and how dense its own segments in the bucket are (doc-27).
    cell: CellId,
    density: Density,
}

/// cladefs: what a cell holds in each of its own segments in the bucket (doc-27, Choosing what to
/// relocate): the bytes of the extents it holds there, of either layer, and the length of each
/// one's data, once known. A segment of its own whose live bytes are under half its data is
/// sparse: its writer relocates what it holds there into its open segment. Kept for its own
/// segments alone: an inherited extent is in its parent's, which the parent's reference keeps
/// whatever the cell does, and an extent stored on its own is wholly live or wholly freed.
///
/// Its live bytes are counted only once its writer first asks ([`Extents::sparse`]), then kept on
/// as they change: counting them walks the bucket's layer whole, which a fork shares with its
/// base's other forks so that opening one costs nothing of what it inherited (TASK-453), and most
/// cells open never relocate, forks, versions and ephemeral cells among them. Lengths it keeps
/// from the start, as segments are acknowledged, one entry a segment. A segment it holds nothing
/// in any more, once counted, is forgotten: its key is written once, and no extent goes back
/// into it.
pub(crate) struct Density {
    /// The prefix of the keys of the cell's own segments (`bucket_cell::layout::segments`).
    prefix: String,
    /// The length of each one's data, of those known.
    lens: HashMap<String, u64>,
    /// The bytes held in each one it holds anything in, once counted.
    live: Option<HashMap<String, u64>>,
}

/// cladefs: a segment of a cell's own in the bucket whose live bytes are under half its data
/// (doc-27's `COMPACT_BELOW`, 0.5, the collector's `compact_below` before it): one its writer
/// relocates what it holds in ([`Extents::sparse`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sparse {
    pub key: String,
    /// The bytes of the extents the cell holds there.
    pub live: u64,
    /// The length of its data.
    pub len: u64,
}

impl Sparse {
    /// The bytes of its data the cell holds no extent in: what relocating out of it frees.
    pub fn dead(&self) -> u64 {
        self.len - self.live
    }
}

impl Density {
    fn new(cell: CellId) -> Self {
        let prefix = bucket_cell::layout::segments(cell);
        Self { prefix, lens: HashMap::new(), live: None }
    }

    /// Whether `key` is of one of the cell's own segments.
    fn counts(&self, key: &str) -> bool {
        key.starts_with(&self.prefix)
    }

    /// An extent held is at `at`.
    fn gained(&mut self, at: &Location) {
        let counts = self.counts(&at.key);
        let Some(live) = self.live.as_mut().filter(|_| counts) else { return };
        match live.get_mut(&at.key) {
            Some(bytes) => *bytes += at.len,
            None => drop(live.insert(at.key.clone(), at.len)),
        }
    }

    /// An extent held at `at` is there no more.
    fn lost(&mut self, at: &Location) {
        let counts = self.counts(&at.key);
        let Some(live) = self.live.as_mut().filter(|_| counts) else { return };
        let bytes = live.get_mut(&at.key).expect("a segment it holds an extent in");
        *bytes -= at.len;
        if *bytes == 0 {
            live.remove(&at.key);
            self.lens.remove(&at.key);
        }
    }

    /// The segment at `key` holds `len` bytes of data: kept, if it is the cell's own, and holds
    /// anything the cell holds, or isn't counted yet.
    fn sized(&mut self, key: &str, len: u64) {
        let held = self.live.as_ref().is_none_or(|live| live.contains_key(key));
        if self.counts(key) && held {
            self.lens.insert(key.to_owned(), len);
        }
    }

    /// Counts the live bytes, from `held`, the places of every extent held, unless counted: what
    /// it keeps on from then on.
    fn count<'a>(&mut self, held: impl Iterator<Item = &'a Location>) {
        if self.live.is_some() {
            return;
        }
        self.live = Some(HashMap::new());
        for at in held {
            self.gained(at);
        }
        let live = self.live.as_ref().expect("counted");
        self.lens.retain(|key, _| live.contains_key(key));
    }

    /// Its segments sparse, but those `pinned` says a kept state reads ([`Extents::sparse`]).
    fn sparse(&self, pinned: impl Fn(&str) -> bool) -> Vec<Sparse> {
        let live = self.live.as_ref().expect("counted");
        let mut sparse: Vec<Sparse> = (live.iter())
            .filter_map(|(key, &live)| {
                let len = *self.lens.get(key)?;
                let sparse = live.saturating_mul(2) < len && !pinned(key);
                sparse.then(|| Sparse { key: key.clone(), live, len })
            })
            .collect();
        sparse.sort_unstable_by(|a, b| b.dead().cmp(&a.dead()).then_with(|| a.key.cmp(&b.key)));
        sparse
    }
}

/// What the cells of this process hold of their own ([`own_held`]).
static OWN_ENTRIES: AtomicU64 = AtomicU64::new(0);
static OWN_RUNS: AtomicU64 = AtomicU64::new(0);

/// What the cells open in this process hold of their own ([`Own`]). What the extent daemon
/// counts of what they hold of their own ([`Own::bytes`]); the decoded states the others share it
/// counts once each.
pub fn own_held() -> Own {
    Own { entries: OWN_ENTRIES.load(Ordering::Relaxed), runs: OWN_RUNS.load(Ordering::Relaxed) }
}

/// What cells hold of their own ([`own_held`]), or one does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Own {
    /// The entries: of their own layers, those they hide of their bucket's, and the bucket's
    /// layers that started from no decoded state, which no other cell shares.
    pub entries: u64,
    /// The runs their writers' maps, each the bucket's layer's shared with others, change of it,
    /// each a path of the persistent map's nodes copied ([`RUN_BYTES`]): the runs of the entries
    /// each hides of the bucket's layer, in its order, which its writer removes or places anew as
    /// it acknowledges or follows them; and its own entries', acknowledged, the cell's newest,
    /// side by side, one more. As many as copy the whole map at most.
    pub runs: u64,
}

impl Own {
    /// The most bytes of the process's heap they hold: [`OWN_ENTRY_BYTES`] an entry,
    /// [`RUN_BYTES`] a run.
    pub fn bytes(&self) -> u64 {
        self.entries * OWN_ENTRY_BYTES + self.runs * RUN_BYTES
    }
}

/// The most bytes of the process's heap an entry of a cell's own layer holds, its writer's map's
/// entry for it once acknowledged included: what the extent daemon counts each of
/// [`Own::entries`] as. A bound, measured by `bench_fork_extent_memory` (cladefs, TASK-453), which
/// fails past it: 613 bytes a block written, committed and uploaded, with what else the cell and
/// its writer keep of it, its location's key a string of its own in each map (TASK-472).
pub const OWN_ENTRY_BYTES: u64 = 1024;

impl Drop for Extents {
    fn drop(&mut self) {
        counted(self.weight(), Own::default());
    }
}

/// Counts for the process that a cell's weight ([`Extents::weight`]) went from `before` to
/// `after`.
fn counted(before: Own, after: Own) {
    for (count, before, after) in
        [(&OWN_ENTRIES, before.entries, after.entries), (&OWN_RUNS, before.runs, after.runs)]
    {
        if after > before {
            count.fetch_add(after - before, Ordering::Relaxed);
        } else if after < before {
            count.fetch_sub(before - after, Ordering::Relaxed);
        }
    }
}

impl Extents {
    /// The extents `bucket` places, the bucket's layer, sharing `layer`, the decoded state it
    /// started from, if it did; none of the cell's own, `cell`'s.
    pub fn over(
        bucket: bucket_cell::recovery::Extents,
        layer: Option<Arc<bucket_cell::State>>,
        cell: CellId,
    ) -> Self {
        #[cfg_attr(not(any(test, feature = "audit")), expect(unused_mut))]
        let mut density = Density::new(cell);
        // cladefs: in the builds of the tests, counted from the start, so that the audit checks
        // what every change does to it, not only what changes after a writer first asks.
        #[cfg(any(test, feature = "audit"))]
        density.count(bucket.values());
        let hidden = HashSet::new();
        let own = HashMap::new();
        let extents = Self { bucket, layer, own, hidden, hidden_runs: 0, cell, density };
        counted(Own::default(), extents.weight());
        extents
    }

    /// What it holds of its own ([`Own`]): the entries of its own layer, those it hides of the
    /// bucket's, and the bucket's layer's, unless it shares a decoded state; and the runs its
    /// writer's map changes of the bucket's layer's, as many as copy it whole at most.
    fn weight(&self) -> Own {
        let unshared = if self.layer.is_none() { self.bucket.len() } else { 0 };
        let entries = (self.own.len() + self.hidden.len() + unshared) as u64;
        let runs = (self.hidden_runs + usize::from(!self.own.is_empty())) as u64;
        let whole = self.bucket.len() as u64 * ENTRY_BYTES / RUN_BYTES;
        Own { entries, runs: runs.min(whole) }
    }

    /// Runs `change` on it, counting what it changes of its weight for the process.
    fn changing<R>(&mut self, change: impl FnOnce(&mut Self) -> R) -> R {
        let before = self.weight();
        let changed = change(self);
        counted(before, self.weight());
        changed
    }

    /// Holds `extent`, of the bucket's layer, there no more, counting the runs those it hides
    /// make: one more, unless it joins one before or after it, or both, one less.
    fn hide(&mut self, extent: ExtentId) {
        use std::ops::Bound::{Excluded, Unbounded};
        if !self.hidden.insert(extent) {
            return;
        }
        let hidden = |next: Option<(&ExtentId, &Location)>| {
            usize::from(next.is_some_and(|(next, _)| self.hidden.contains(next)))
        };
        let before = hidden(self.bucket.range(..extent).next_back());
        let after = hidden(self.bucket.range((Excluded(extent), Unbounded)).next());
        self.hidden_runs = self.hidden_runs + 1 - before - after;
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

    /// `extent`'s entry, if it holds it, borrowed from the layer that holds it ([`Entry`]).
    pub fn get(&self, extent: &ExtentId) -> Option<Entry<'_>> {
        match self.own.get(extent) {
            Some(entry) => Some(Entry::Own(entry)),
            None => self.in_bucket(extent).map(Entry::Bucket),
        }
    }

    /// cladefs: `extent`, held, its data at `data` now, as a seal, an acknowledgement or a
    /// relocation places it: returns where it was. One of the bucket's layer is the cell's own
    /// layer's from then on, over it.
    pub fn place(&mut self, extent: &ExtentId, data: Data) -> Data {
        let Some(entry) = self.own.get_mut(extent) else {
            // Held in the bucket's layer still: its own over it, as a relocation places it.
            let at = self.in_bucket(extent).expect("an extent placed is held").clone();
            self.insert(*extent, Extent { len: at.len, data });
            return Data::Bucket(at);
        };
        let was = std::mem::replace(&mut entry.data, data);
        if let Data::Bucket(at) = &was {
            self.density.lost(at);
        }
        if let Data::Bucket(at) = &self.own[extent].data {
            self.density.gained(at);
        }
        was
    }

    /// cladefs: `extent`, if held in the bucket, is held there no more, as its density counts it:
    /// what a change of its entry begins with.
    fn unlive(&mut self, extent: &ExtentId) {
        let at = match self.own.get(extent) {
            Some(Extent { data: Data::Bucket(at), .. }) => Some(at),
            Some(_) => None,
            None => self.bucket.get(extent).filter(|_| !self.hidden.contains(extent)),
        };
        if let Some(at) = at {
            self.density.lost(at);
        }
    }

    /// cladefs: the segment of its own at `key`, if it holds anything there, holds `len` bytes of
    /// data ([`Density`]).
    pub fn sized(&mut self, key: &str, len: u64) {
        self.density.sized(key, len);
    }

    /// cladefs: its own segments in the bucket that are sparse ([`Sparse`]), but those `pinned`
    /// says a kept state reads, which relocating out of would only copy what they keep anyway:
    /// most dead bytes first, then by key, so that a round's budget goes where it frees most, in
    /// an order no map's decides. Their live bytes counted the first time it is asked
    /// ([`Density`]).
    pub fn sparse(&mut self, pinned: impl Fn(&str) -> bool) -> Vec<Sparse> {
        if self.density.live.is_none() {
            let mut density = std::mem::replace(&mut self.density, Density::new(self.cell));
            density.count(self.iter().filter_map(|(_, entry)| match entry {
                Entry::Own(Extent { data: Data::Bucket(at), .. }) | Entry::Bucket(at) => Some(at),
                Entry::Own(_) => None,
            }));
            self.density = density;
        }
        self.density.sparse(pinned)
    }

    /// `extent`'s entry, which a write to it changes: its own, or `make`'s, entered, if it holds
    /// none. An extent written is open, or new, the cell's: never in the bucket's layer.
    pub fn written(&mut self, extent: ExtentId, make: impl FnOnce() -> Extent) -> &mut Extent {
        debug_assert!(self.in_bucket(&extent).is_none(), "{extent:?}, written, is the bucket's");
        if !self.own.contains_key(&extent) {
            self.changing(|extents| extents.own.insert(extent, make()));
        }
        self.own.get_mut(&extent).expect("entered")
    }

    /// Holds `extent` at `entry`, in its own layer, over the bucket's.
    pub fn insert(&mut self, extent: ExtentId, entry: Extent) {
        self.unlive(&extent);
        if let Data::Bucket(at) = &entry.data {
            self.density.gained(at);
        }
        self.changing(|extents| {
            if extents.bucket.contains_key(&extent) {
                extents.hide(extent);
            }
            extents.own.insert(extent, entry);
        });
    }

    /// Holds `extent` no more: returns its entry, if it held it.
    pub fn remove(&mut self, extent: &ExtentId) -> Option<Extent> {
        self.unlive(extent);
        self.changing(|extents| {
            if let Some(entry) = extents.own.remove(extent) {
                return Some(entry);
            }
            let entry = remote(extents.in_bucket(extent)?);
            extents.hide(*extent);
            Some(entry)
        })
    }

    /// Names `extent`, if it holds it in the bucket, at `to`, where the collector moved it, if it
    /// names it on its way there ([`Data::follow`]): in its own layer, if the bucket's places it.
    pub fn follow(&mut self, extent: &ExtentId, to: &Location) {
        if let Some(entry) = self.own.get(extent) {
            if let Data::Bucket(at) = &entry.data
                && bucket_cell::layout::further(&at.key, &to.key)
            {
                self.place(extent, Data::Bucket(to.clone()));
            }
            return;
        }
        let Some(at) = self.in_bucket(extent) else { return };
        if bucket_cell::layout::further(&at.key, &to.key) {
            let len = at.len;
            self.insert(*extent, Extent { len, data: Data::Bucket(to.clone()) });
        }
    }

    /// Its extents, with their entries, in no order.
    pub fn iter(&self) -> impl Iterator<Item = (ExtentId, Entry<'_>)> {
        let own = self.own.iter().map(|(extent, entry)| (*extent, Entry::Own(entry)));
        own.chain(self.bucket_entries().map(|(extent, at)| (extent, Entry::Bucket(at))))
    }

    /// Its extents, with their lengths, in no order.
    pub fn lens(&self) -> impl Iterator<Item = (ExtentId, u64)> {
        self.iter().map(|(extent, entry)| (extent, entry.len()))
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

    /// cladefs: panics unless what its density counts held in each of the cell's own segments, once
    /// counted, is what it holds there, recounted from both layers; and it knows the lengths of
    /// none it holds nothing in ([`Density`]).
    #[cfg(any(test, feature = "audit"))]
    pub fn audit(&self) {
        let Some(counted) = &self.density.live else { return };
        let mut live: HashMap<&str, u64> = HashMap::new();
        for (_, entry) in self.iter() {
            let at = match entry {
                Entry::Own(Extent { data: Data::Bucket(at), .. }) | Entry::Bucket(at) => at,
                Entry::Own(_) => continue,
            };
            if self.density.counts(&at.key) {
                *live.entry(at.key.as_str()).or_default() += at.len;
            }
        }
        let counted: HashMap<&str, u64> =
            counted.iter().map(|(key, live)| (key.as_str(), *live)).collect();
        assert_eq!(counted, live, "the bytes held in each of the cell's own segments");
        for key in self.density.lens.keys() {
            assert!(live.contains_key(key.as_str()), "{key}'s length kept, nothing held there");
        }
    }

    /// cladefs: whether it shares its bucket's layer with `other`'s: one map, not two alike.
    #[cfg(any(test, feature = "testing"))]
    pub fn shares_bucket_with(&self, other: &Self) -> bool {
        self.bucket.ptr_eq(&other.bucket)
    }
}

/// An extent's entry, as the layer of [`Extents`] that holds it has it, borrowed: the cell's own,
/// or where the bucket's layer places it, which is made into an entry ([`Entry::to_extent`]) only
/// where one is wanted whole (TASK-453). Lookups that want its length, or whether it is held, as
/// every read of an inherited extent does under the cell's lock, so allocate nothing: making an
/// entry of the bucket's location copies its key.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Entry<'a> {
    /// The cell's own.
    Own(&'a Extent),
    /// Where the bucket's layer places it: in the bucket, read there.
    Bucket(&'a Location),
}

impl<'a> Entry<'a> {
    /// Its length.
    pub fn len(self) -> u64 {
        match self {
            Entry::Own(entry) => entry.len,
            Entry::Bucket(at) => at.len,
        }
    }

    /// The cell's own entry, if it is the cell's own: those of the bucket's layer are in the
    /// bucket, their data [`Data::Bucket`].
    pub fn own(self) -> Option<&'a Extent> {
        match self {
            Entry::Own(entry) => Some(entry),
            Entry::Bucket(_) => None,
        }
    }

    /// The entry, whole: borrowed if the cell's own, made of where the bucket's layer places it
    /// otherwise.
    pub fn into_cow(self) -> Cow<'a, Extent> {
        match self {
            Entry::Own(entry) => Cow::Borrowed(entry),
            Entry::Bucket(at) => Cow::Owned(remote(at)),
        }
    }

    /// The entry, whole, owned.
    pub fn to_extent(self) -> Extent {
        self.into_cow().into_owned()
    }
}

/// The entry of an extent the bucket holds at `location`.
fn remote(location: &Location) -> Extent {
    Extent { len: location.len, data: Data::Bucket(location.clone()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Piece, SegmentId};

    /// The cell the tests' extents are of, whose segments the bucket holds them in.
    fn cell() -> CellId {
        bucket_cell::layout::test_cell(1)
    }

    /// Where the bucket holds an extent: in a segment of the base's, at `offset`.
    fn at(offset: u64) -> Location {
        let key = bucket_cell::layout::segment(bucket_cell::layout::test_cell(1), 1, 1);
        Location { key, offset, len: 4096 }
    }

    fn extent(sequence: u64) -> ExtentId {
        ExtentId::new(1, sequence)
    }

    fn pieces(number: u64) -> Extent {
        let piece =
            Piece { segment: SegmentId { cell: 2, number }, offset: 0, extent_offset: 0, len: 8 };
        Extent { len: 8, data: Data::Pieces(vec![piece]) }
    }

    /// The bucket's layer, as the cell opened: extents 0 to 9, in the bucket.
    fn bucket() -> bucket_cell::recovery::Extents {
        (0..10).map(|sequence| (extent(sequence), at(sequence * 10))).collect()
    }

    /// What the process counts its cells hold of their own more than `then`.
    fn more_than(then: Own) -> Own {
        let now = own_held();
        Own { entries: now.entries - then.entries, runs: now.runs - then.runs }
    }

    fn held(extents: &Extents) -> Vec<(ExtentId, Extent)> {
        let mut held: Vec<_> =
            extents.iter().map(|(extent, entry)| (extent, entry.to_extent())).collect();
        held.sort_by_key(|(extent, _)| *extent);
        held
    }

    /// A cell's extents read through its own layer to the bucket's; what it writes, frees,
    /// overlays and follows is its own, the bucket's layer unchanged, shared with another open cell
    /// opened from the same decoded state, which holds it all still; and the process counts what
    /// each holds of its own, until it is dropped: a bucket's layer that shares no decoded state
    /// among it.
    #[test]
    fn cells_layer_their_own_over_the_buckets() {
        let base = bucket();
        let decoded = bucket_cell::State::of(bucket_cell::layout::test_cell(1), base.clone());
        let layer = Some(Arc::new(decoded));
        let counted = own_held();
        let (mut one, other) = (
            Extents::over(base.clone(), layer.clone(), cell()),
            Extents::over(base.clone(), layer, cell()),
        );
        assert_eq!(own_held(), counted);
        assert!(one.shares_bucket_with(&other));
        assert_eq!(one.len(), 10);
        assert_eq!(one.get(&extent(3)).map(Entry::to_extent), Some(remote(&at(30))));

        // Its own: written, freed, overlaid, followed.
        one.written(extent(20), || pieces(1)).len = 8;
        assert_eq!(one.remove(&extent(4)), Some(remote(&at(40))));
        assert_eq!(one.remove(&extent(4)), None);
        one.insert(extent(5), pieces(2));
        let key = bucket_cell::layout::moves(&at(60).key, 1).unwrap();
        let moved = Location { key, offset: 0, len: 4096 };
        one.follow(&extent(6), &moved);
        let elsewhere = bucket_cell::layout::segment(bucket_cell::layout::test_cell(1), 2, 1);
        let unrelated = Location { key: elsewhere, offset: 0, len: 4096 };
        one.follow(&extent(7), &unrelated);
        one.follow(&extent(4), &moved);
        assert_eq!(one.remove(&extent(20)), Some(pieces(1)));

        assert_eq!(one.len(), 9);
        assert!(one.get(&extent(4)).is_none() && one.get(&extent(20)).is_none());
        assert_eq!(one.get(&extent(5)).map(Entry::to_extent), Some(pieces(2)));
        assert_eq!(one.get(&extent(6)).unwrap().to_extent().data, Data::Bucket(moved.clone()));
        assert!(
            matches!(one.get(&extent(7)), Some(Entry::Bucket(location)) if *location == at(70))
        );
        let mut expected: Vec<_> =
            base.iter().map(|(extent, location)| (*extent, remote(location))).collect();
        expected.retain(|(held, _)| *held != extent(4));
        expected[4].1 = pieces(2);
        expected[5].1 = Extent { len: 4096, data: Data::Bucket(moved) };
        assert_eq!(held(&one), expected);
        let lens: Vec<_> = {
            let mut lens: Vec<_> = one.lens().collect();
            lens.sort();
            lens
        };
        let entries: Vec<_> = expected.iter().map(|(extent, entry)| (*extent, entry.len)).collect();
        assert_eq!(lens, entries);
        assert_eq!(one.own().count(), 2);
        // Its own: 5 and 6; hidden: 4, 5 and 6, one run, and its own, one more: as many as copy
        // its writer's map of 10 whole at most, one.
        assert_eq!(more_than(counted), Own { entries: 5, runs: 1 });

        // The other's, and the bucket's layer, unchanged.
        assert!(one.shares_bucket_with(&other));
        let whole: Vec<_> =
            base.iter().map(|(extent, location)| (*extent, remote(location))).collect();
        assert_eq!(held(&other), whole);
        drop(one);
        assert_eq!(own_held(), counted);
        let alone = Extents::over(base, None, cell());
        assert_eq!(more_than(counted), Own { entries: 10, runs: 0 });
        drop(alone);
        assert_eq!(own_held(), counted);
    }
    /// What a cell hides of its bucket's layer is counted by the runs it makes there, side by
    /// side in its order, each a path its writer's map, the bucket's layer's shared, copies as it
    /// removes or places them anew; its own entries, acknowledged, one more; and as many as copy
    /// the map whole at most: 1,000 entries, of 512 bytes, 125 runs of 4 KiB.
    #[test]
    fn cells_count_the_runs_their_writers_change() {
        let base: bucket_cell::recovery::Extents =
            (0..1000).map(|sequence| (extent(sequence), at(sequence * 10))).collect();
        let layer = bucket_cell::State::of(bucket_cell::layout::test_cell(1), base.clone());
        let counted = own_held();
        let mut extents = Extents::over(base, Some(Arc::new(layer)), cell());
        let runs = |extents: &Extents| extents.weight().runs;
        extents.remove(&extent(10));
        extents.remove(&extent(12));
        assert_eq!(runs(&extents), 2, "two apart");
        extents.insert(extent(11), pieces(1));
        assert_eq!(runs(&extents), 2, "one between them joins them, and its own, one more");
        extents.remove(&extent(13));
        extents.remove(&extent(9));
        extents.remove(&extent(9));
        assert_eq!(runs(&extents), 2, "beside them");
        extents.remove(&extent(0));
        extents.remove(&extent(999));
        assert_eq!(runs(&extents), 4, "at either end");
        for sequence in (100..1000).step_by(2) {
            extents.remove(&extent(sequence));
        }
        assert_eq!(runs(&extents), 125, "as many as copy it whole");
        assert_eq!(more_than(counted), Own { entries: 458, runs: 125 });
        drop(extents);
        assert_eq!(own_held(), counted);
    }
}
