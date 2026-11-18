// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: what a host knows of its bucket without asking it again (TASK-449): a restore, a fork
//! of a version and its child's first open, asked the bucket 14 requests, 13 one after another, for
//! what the host had just read or written itself. Two kinds of knowledge, each safe for its own
//! reason (doc-7, What a host knows):
//!
//! - **Facts that never change once seen**: the epoch of the writer of a cell's segment, once a
//!   head names it ([`Known::epoch`]). A head's runs of epochs only grow: a takeover goes on from
//!   its end, a deletion keeps what it names, and nothing renames a segment named (specs/bucket's
//!   NamedEpochsStay). So an origin's checkpoint, at the key of that epoch, is found by its key
//!   with no read of the origin's head, whichever host or however long after.
//! - **What a host learned of a cell it is about to open** ([`Learned`]): its record as its own
//!   fork or take left it, its cell's object as its fork held it, and whether it has no head. The
//!   cell's store, opened next, takes it ([`Known::take`]) in place of its first reads. Each is
//!   what a read at the moment of the write would have found: so using it is that read, made
//!   earlier, which every step after allows for: the head's taking, and the cell's holding when it
//!   writes, are conditional on the version found, going on from what is there if another's write
//!   came between; what decides a step that writes nothing is read again once the head is taken
//!   (`Writer::take`): the record, as after a first read from before a deletion (specs/bucket's
//!   staleTake, OpenGone), and the cell, if its holding started from what the fork left
//!   ([`crate::Taker::remembered`], TASK-449's review: another placement's holding since refuses
//!   the opening). Wrong, it costs requests, never safety; it lives in the host's memory alone,
//!   taken by the opening it was learned for as that begins, and goes with the process.

use crate::bucket::Object;
use crate::layout::CellId;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

/// What a host learned of a cell before it opens it ([`Known`]): its head, as this host's last
/// write of it left it, of the version that write made: what the store's taking starts from.
#[derive(Clone, Debug)]
pub struct Learned {
    pub head: Object,
}

/// What a host knows of its bucket ([`crate::Bucket::known`]; see the module's docs).
#[derive(Default)]
pub struct Known {
    epochs: Mutex<Bounded<(CellId, u64), u64>>,
    learned: Mutex<Bounded<CellId, Learned>>,
}

impl Known {
    /// How many segments' epochs it keeps: an origin each, of the versions (bases, VM snapshots') the
    /// host forks.
    const EPOCHS: usize = 1 << 16;

    /// How many cells' learnings it keeps, the oldest going first: those of forks not opened
    /// (yet), which a host's restores open at once.
    const LEARNED: usize = 1 << 12;

    /// The epoch of the writer of segment `segment` of `cell`, if a head was seen naming it.
    pub fn epoch(&self, cell: CellId, segment: u64) -> Option<u64> {
        self.epochs.lock().unwrap().get(&(cell, segment)).copied()
    }

    /// Notes that a head of `cell` names its segment `segment` as of the writer of `epoch`: so it
    /// does for good.
    pub fn named(&self, cell: CellId, segment: u64, epoch: u64) {
        self.epochs.lock().unwrap().insert((cell, segment), epoch, Self::EPOCHS);
    }

    /// Notes what this host learned of `cell`, which it is about to open.
    pub fn learn(&self, cell: CellId, learned: Learned) {
        self.learned.lock().unwrap().insert(cell, learned, Self::LEARNED);
    }

    /// What this host learned of `cell`, if anything, which its opening takes: once.
    pub fn take(&self, cell: CellId) -> Option<Learned> {
        self.learned.lock().unwrap().remove(&cell)
    }
}

/// A map of at most so many entries, the oldest put going first.
struct Bounded<K, V> {
    map: HashMap<K, V>,
    order: VecDeque<K>,
}

impl<K, V> Default for Bounded<K, V> {
    fn default() -> Self {
        Self { map: HashMap::new(), order: VecDeque::new() }
    }
}

impl<K: std::hash::Hash + Eq + Clone, V> Bounded<K, V> {
    fn get(&self, key: &K) -> Option<&V> {
        self.map.get(key)
    }

    fn insert(&mut self, key: K, value: V, most: usize) {
        if self.map.insert(key.clone(), value).is_none() {
            self.order.push_back(key);
        }
        while self.map.len() > most {
            let Some(oldest) = self.order.pop_front() else { break };
            self.map.remove(&oldest);
        }
    }

    fn remove(&mut self, key: &K) -> Option<V> {
        let value = self.map.remove(key)?;
        self.order.retain(|kept| kept != key);
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::test_cell;

    /// What it keeps is bounded, the oldest going first; a learning is taken once.
    #[test]
    fn knowledge_is_bounded_and_learnings_taken_once() {
        let mut bounded = Bounded::default();
        for key in 0..5 {
            bounded.insert(key, key * 10, 3);
        }
        assert_eq!(
            (bounded.get(&1), bounded.get(&2), bounded.get(&4)),
            (None, Some(&20), Some(&40))
        );
        assert_eq!(bounded.map.len(), 3);

        let known = Known::default();
        let cell = test_cell(3);
        known.named(cell, 7, 2);
        assert_eq!((known.epoch(cell, 7), known.epoch(cell, 6)), (Some(2), None));
        let head = crate::layout::encode(&crate::layout::Head::of(crate::record::Made::Root, 1));
        let head = Object::written(&crate::layout::head(cell), head, Default::default(), 0);
        known.learn(cell, Learned { head: head.clone() });
        assert!(known.take(cell).is_some_and(|learned| learned.head.bytes == head.bytes));
        assert!(known.take(cell).is_none());
    }
}
