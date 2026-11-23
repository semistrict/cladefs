// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the extents freed whose data stays ([`Kept`]), for the uploader, indexed so that what a
//! commit does with them takes the time of what it lets go of, not of all of them: the uploader
//! keeps every free until it has uploaded the commit, thousands of them behind object storage
//! (TASK-41).

use crate::layout::Extent;
use std::collections::{BTreeSet, HashMap};
use storage_device::ExtentId;

/// An extent freed whose data stays, sealed: for the uploader to read, until it releases the
/// commit that freed it. A free's is kept until the commit that frees it decides.
pub(crate) struct Kept {
    pub entry: Extent,
    /// The epoch of the commit that freed it: [`PENDING`] until one does.
    pub freed: u64,
    /// Whether the uploader still needs it.
    pub uploader: bool,
}

/// cladefs: the epoch a free is kept as freed by until the commit that makes it numbers it
/// ([`KeptExtents::freed_by`]): one past every commit, so that no trim, which goes by the commits
/// published, nor the uploader's release, which goes by those acknowledged, lets it go first. Not
/// the next commit's number: a commit made between that makes no free, a relocation's (doc-27) or
/// a resize's, would take it.
pub(crate) const PENDING: u64 = u64::MAX;

/// The extents kept, by ID, and by the commits that freed them.
#[derive(Default)]
pub(crate) struct KeptExtents {
    kept: HashMap<ExtentId, Kept>,
    /// All of them, by the epoch of the commit that freed each.
    by_freed: BTreeSet<(u64, ExtentId)>,
    /// Those the uploader doesn't need, by the epoch of the commit that freed each: those a trim
    /// may let go of.
    released: BTreeSet<(u64, ExtentId)>,
    /// The latest commit the uploader released, whose frees and those before it it needs no more.
    released_to: u64,
    /// The bytes of their data.
    bytes: u64,
}

impl KeptExtents {
    pub fn get(&self, extent: &ExtentId) -> Option<&Kept> {
        self.kept.get(extent)
    }

    /// The bytes of the data of the extents kept.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Keeps `extent`, freed as `kept` says.
    pub fn insert(&mut self, extent: ExtentId, kept: Kept) {
        debug_assert!(kept.freed > self.released_to, "a free after the commits released");
        if let Some(old) = self.kept.remove(&extent) {
            self.unindex(extent, &old);
        }
        self.by_freed.insert((kept.freed, extent));
        if !kept.uploader {
            self.released.insert((kept.freed, extent));
        }
        self.bytes += kept.entry.len;
        self.kept.insert(extent, kept);
    }

    /// cladefs: `extent`, if kept as freed by a commit to come ([`PENDING`]), is freed by commit
    /// `epoch`, which makes its free.
    pub fn freed_by(&mut self, extent: ExtentId, epoch: u64) {
        let Some(kept) = self.kept.get_mut(&extent).filter(|kept| kept.freed == PENDING) else {
            return;
        };
        kept.freed = epoch;
        let uploader = kept.uploader;
        self.by_freed.remove(&(PENDING, extent));
        self.by_freed.insert((epoch, extent));
        if !uploader {
            self.released.remove(&(PENDING, extent));
            self.released.insert((epoch, extent));
        }
    }

    /// The uploader needs those the commits up to `epoch` freed no more.
    pub fn release(&mut self, epoch: u64) {
        if epoch <= self.released_to {
            return;
        }
        let first = (self.released_to + 1, ExtentId::new(0, 0));
        let last = (epoch, ExtentId::new(u64::MAX, u64::MAX));
        for &(freed, extent) in self.by_freed.range(first..=last) {
            let kept = self.kept.get_mut(&extent).unwrap();
            if kept.uploader {
                kept.uploader = false;
                self.released.insert((freed, extent));
            }
        }
        self.released_to = epoch;
    }

    /// Takes out those the commits up to `published` freed that the uploader needs no more, and
    /// returns them.
    pub fn trim(&mut self, published: u64) -> Vec<(ExtentId, Kept)> {
        let last = (published, ExtentId::new(u64::MAX, u64::MAX));
        let gone: Vec<ExtentId> = self.released.range(..=last).map(|(_, extent)| *extent).collect();
        (gone.into_iter())
            .map(|extent| {
                let kept = self.kept.remove(&extent).unwrap();
                self.unindex(extent, &kept);
                (extent, kept)
            })
            .collect()
    }

    /// `extent`, kept as `kept` says, is kept no more.
    fn unindex(&mut self, extent: ExtentId, kept: &Kept) {
        self.by_freed.remove(&(kept.freed, extent));
        self.released.remove(&(kept.freed, extent));
        self.bytes -= kept.entry.len;
    }

    /// cladefs: those kept, in no order, to change where their data is, which is all of them a
    /// change may change (TASK-165.8).
    pub fn entries_mut(&mut self) -> impl Iterator<Item = (&ExtentId, &mut Kept)> {
        self.kept.iter_mut()
    }

    /// cladefs: those kept, in no order, for [`State::audit`](crate::state::State::audit).
    #[cfg(any(test, feature = "audit"))]
    pub fn iter(&self) -> impl Iterator<Item = (&ExtentId, &Kept)> {
        self.kept.iter()
    }

    /// cladefs: panics unless its indexes, and what it counts of them, are what it keeps, recounted
    /// (TASK-160.16): all of them by the commits that freed them, those the uploader doesn't need,
    /// which are all the commits it released freed, and their bytes.
    #[cfg(any(test, feature = "audit"))]
    pub fn audit(&self) {
        let by_freed: BTreeSet<(u64, ExtentId)> =
            self.kept.iter().map(|(extent, kept)| (kept.freed, *extent)).collect();
        let released: BTreeSet<(u64, ExtentId)> = (self.kept.iter())
            .filter(|(_, kept)| !kept.uploader)
            .map(|(extent, kept)| (kept.freed, *extent))
            .collect();
        assert_eq!(self.by_freed, by_freed, "the frees kept by the commits that freed them");
        assert_eq!(self.released, released, "the frees kept the uploader released");
        for (extent, kept) in &self.kept {
            assert!(kept.freed > self.released_to || !kept.uploader, "{extent:?} kept, released");
        }
        let bytes: u64 = self.kept.values().map(|kept| kept.entry.len).sum();
        assert_eq!(self.bytes, bytes, "the bytes of the frees kept");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Data;

    fn freed(freed: u64, uploader: bool, len: u64) -> Kept {
        let location = extent_frame::segment::Location { key: "k".to_owned(), offset: 0, len };
        Kept { entry: Extent { len, data: Data::Bucket(location) }, freed, uploader }
    }

    fn extent(sequence: u64) -> ExtentId {
        ExtentId::new(1, sequence)
    }

    /// An extent kept again is kept as the later free says: its bytes and its commit counted once.
    #[test]
    fn extents_kept_again_count_once() {
        let mut extents = KeptExtents::default();
        extents.insert(extent(1), freed(1, false, 4096));
        extents.insert(extent(1), freed(2, false, 8192));
        assert_eq!(extents.bytes(), 8192);
        let trimmed = |extents: &mut KeptExtents, published| -> Vec<ExtentId> {
            extents.trim(published).into_iter().map(|(extent, _)| extent).collect()
        };
        assert_eq!(trimmed(&mut extents, 1), []);
        assert_eq!(trimmed(&mut extents, 2), [extent(1)]);
        assert_eq!(extents.bytes(), 0);
    }
}
