// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: what a host knows of its bucket without asking it again (TASK-449): a restore, a fork
//! of a point and its child's first open, asked the bucket 14 requests, 13 one after another, for
//! what the host had just read or written itself. Two kinds of knowledge, each safe for its own
//! reason (doc-7, What a host knows):
//!
//! - **Facts that never change once seen**: the epoch of the writer of a branch's segment, once a
//!   head names it ([`Known::epoch`]). A head's runs of epochs only grow: a takeover goes on from
//!   its end, a deletion keeps what it names, and nothing renames a segment named (specs/bucket's
//!   NamedEpochsStay). So a fork point's checkpoint, at the key of that epoch, is found by its key
//!   with no read of the origin's head, whichever host or however long after.
//! - **What a host learned of a branch it is about to open** ([`Learned`]): its record as its own
//!   fork or take left it, its cell's object as its fork held it, and whether it has no head. The
//!   branch's store, opened next, takes it ([`Known::take`]) in place of its first reads. Each is
//!   what a read at the moment of the write would have found: so using it is that read, made
//!   earlier, which every step after allows for: the head's taking and the cell's are conditional
//!   on the version found, going on from what is there if another's write came between, and the
//!   record is read again once the head is taken (`Writer::take`), as after a first read from
//!   before a deletion (specs/bucket's staleTake, OpenGone). Wrong, it costs requests, never
//!   safety; it lives in the host's memory alone, and goes with the process.

use crate::bucket::Object;
use crate::layout::Branch;
use crate::record::Record;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

/// What a host learned of a branch before it opens it ([`Known`]).
#[derive(Clone, Debug)]
pub struct Learned {
    /// Its record, as this host's last write or read of it left it.
    pub record: Record,
    /// Its cell's object, as this host held it last, if it did: what the store's holding of the
    /// cell starts from, written, of the version that write made.
    pub cell: Option<Object>,
    /// Whether it has no head: a clone this host made, which no writer took yet.
    pub headless: bool,
}

/// What a host knows of its bucket ([`crate::Bucket::known`]; see the module's docs).
#[derive(Default)]
pub struct Known {
    epochs: Mutex<Bounded<(Branch, u64), u64>>,
    learned: Mutex<Bounded<Branch, Learned>>,
}

impl Known {
    /// How many segments' epochs it keeps: a fork point each, of the bases and snapshots the host
    /// restores from.
    const EPOCHS: usize = 1 << 16;

    /// How many branches' learnings it keeps, the oldest going first: those of forks not opened
    /// (yet), which a host's restores open at once.
    const LEARNED: usize = 1 << 12;

    /// The epoch of the writer of segment `seq` of `branch`, if a head was seen naming it.
    pub fn epoch(&self, branch: Branch, seq: u64) -> Option<u64> {
        self.epochs.lock().unwrap().get(&(branch, seq)).copied()
    }

    /// Notes that a head of `branch` names its segment `seq` as of the writer of `epoch`: so it
    /// does for good.
    pub fn named(&self, branch: Branch, seq: u64, epoch: u64) {
        self.epochs.lock().unwrap().insert((branch, seq), epoch, Self::EPOCHS);
    }

    /// Notes what this host learned of `branch`, which it is about to open.
    pub fn learn(&self, branch: Branch, learned: Learned) {
        self.learned.lock().unwrap().insert(branch, learned, Self::LEARNED);
    }

    /// What this host learned of `branch`, if anything, which its opening takes: once.
    pub fn take(&self, branch: Branch) -> Option<Learned> {
        self.learned.lock().unwrap().remove(&branch)
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
    use crate::layout::test_branch;

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
        let branch = test_branch(3);
        known.named(branch, 7, 2);
        assert_eq!((known.epoch(branch, 7), known.epoch(branch, 6)), (Some(2), None));
        let record = crate::record::Kind::Root.of(1);
        known.learn(branch, Learned { record, cell: None, headless: true });
        assert!(known.take(branch).is_some_and(|learned| learned.headless));
        assert!(known.take(branch).is_none());
    }
}
