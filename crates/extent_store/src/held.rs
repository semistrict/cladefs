// Copyright 2026 The fxdax Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! fxdax: the parts of segments packed as they fill, held in memory until their framings
//! ([`HeldParts`], TASK-400): what a framing in a copy writes of each, so that it reads none of
//! them back from its raw file.
//!
//! A framing in a copy read its segment's data back as it closed, and packed it: on a host short
//! of memory, the long mode's 16 GiB at its limit, the host had let the raw file's pages go, and
//! the framing read them back from a disk eight VMs and their bucket kept busy, up to 3.5 s. A part
//! of a segment that looks as if it will be framed in a copy is packed as soon as it is written
//! whole, its bytes in the page cache still, and held here, compressed (or raw, if it doesn't
//! compress), until its segment's framing takes it.
//!
//! A part is packed off the framer's thread, at the host's lowest priority (`framing::pack_ahead`):
//! held pending until it is, its framing finding it not packed yet, or never, as the host's CPUs
//! were busy, reads and packs it itself, as before, and what packing it ahead comes to is let go.
//! A look ahead starved of CPU leaves parts unheld; it never holds up a framing.
//!
//! A part held is derived from the raw file, which stays the source of truth until the framed file
//! is durable: one let go is read back and packed by its framing, as before. So what holds parts
//! may let any go at any time, as the host's memory runs short ([`HeldParts::evict_to`]): a
//! daemon's governor, before any tenant's cached pages (`fxdax`'s extentd). Newest first: a
//! segment's framing is near for those held longest, and those held last are those whose raw pages
//! the host is likeliest to hold still, as it lets the least recently used go first.
//!
//! The stores of a process share one, which they take as they open
//! ([`Opening::held`](crate::Opening::held)); without one, a store holds its parts in one of its
//! own, which nothing makes let go of any: the parts of its open segment and of those being framed,
//! at most.

use crate::framing::Stored;
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};

/// fxdax: the parts of segments packed ahead of their framings, held in memory for the stores that
/// share it, as the module's docs say.
#[derive(Default)]
pub struct HeldParts {
    held: Mutex<Held>,
}

#[derive(Default)]
struct Held {
    /// The parts held, by the order they were held in.
    parts: BTreeMap<u64, Stored>,
    /// Those to hold once packed ([`Pending`]).
    pending: HashSet<u64>,
    next: u64,
    counts: HeldCounts,
}

/// fxdax: what parts [`HeldParts`] holds, and held, in bytes of memory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeldCounts {
    /// Those held now.
    pub bytes: u64,
    /// The most held at once.
    pub peak: u64,
    /// Those held in all, and of them, those their framings took, and those let go to make room.
    pub held: u64,
    pub taken: u64,
    pub evicted: u64,
    /// The parts let go to make room.
    pub evicted_parts: u64,
    /// The parts their framings found pending, packed not yet, and those never packed, the packing
    /// behind ([`Pending::skip`]): their framings read and packed them.
    pub late_parts: u64,
    pub skipped_parts: u64,
}

impl std::fmt::Debug for HeldParts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HeldParts({:?})", self.counts())
    }
}

impl HeldParts {
    /// None held.
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// The bytes of memory the parts held take.
    pub fn bytes(&self) -> u64 {
        self.held.lock().unwrap().counts.bytes
    }

    /// What it holds, and held.
    pub fn counts(&self) -> HeldCounts {
        self.held.lock().unwrap().counts
    }

    /// Lets parts go, those held last first, until those left take `target` bytes at most, and
    /// returns the bytes it let go of: their framings read them back.
    pub fn evict_to(&self, target: u64) -> u64 {
        let mut evicted = Vec::new();
        let freed = {
            let mut held = self.held.lock().unwrap();
            let before = held.counts.bytes;
            while held.counts.bytes > target {
                let (_, part) = held.parts.pop_last().expect("bytes held, of parts held");
                held.counts.bytes -= part.bytes();
                held.counts.evicted += part.bytes();
                held.counts.evicted_parts += 1;
                evicted.push(part);
            }
            before - held.counts.bytes
        };
        // Freed holding no lock.
        drop(evicted);
        freed
    }

    /// Holds `part`, until its framing takes it, or it is let go of.
    pub(crate) fn hold(self: &Arc<Self>, part: Stored) -> Hold {
        let (hold, pending) = self.pend();
        pending.fill(part);
        hold
    }

    /// A part to hold once it is packed, which `Pending` fills, if its framing hasn't taken it
    /// meanwhile, and nothing dropped it.
    pub(crate) fn pend(self: &Arc<Self>) -> (Hold, Pending) {
        let mut held = self.held.lock().unwrap();
        let key = held.next;
        held.next += 1;
        held.pending.insert(key);
        let parts = self.clone();
        (Hold { key, parts: parts.clone() }, Pending { key, parts, done: false })
    }

    /// The part held as `key`, if it still is, held no more; counted taken, if `taken`. One
    /// pending is held no more either: counted late, if `taken`.
    fn remove(&self, key: u64, taken: bool) -> Option<Stored> {
        let mut held = self.held.lock().unwrap();
        let Some(part) = held.parts.remove(&key) else {
            if held.pending.remove(&key) && taken {
                held.counts.late_parts += 1;
            }
            return None;
        };
        held.counts.bytes -= part.bytes();
        if taken {
            held.counts.taken += part.bytes();
        }
        Some(part)
    }
}

/// fxdax: a part to hold once packed ([`HeldParts::pend`]), as the packing ahead fills it, or skips
/// it. Dropped neither, it is held never.
pub(crate) struct Pending {
    key: u64,
    parts: Arc<HeldParts>,
    done: bool,
}

impl Pending {
    /// Whether it is still to be held: else its framing took it, or its segment dropped it, and
    /// packing it is for nothing.
    pub fn wanted(&self) -> bool {
        self.parts.held.lock().unwrap().pending.contains(&self.key)
    }

    /// Holds `part`, packed, if it is still to be held; else lets it go.
    pub fn fill(mut self, part: Stored) {
        self.done = true;
        let mut held = self.parts.held.lock().unwrap();
        if !held.pending.remove(&self.key) {
            drop(held);
            // Freed holding no lock.
            drop(part);
            return;
        }
        let bytes = part.bytes();
        held.counts.bytes += bytes;
        held.counts.held += bytes;
        held.counts.peak = held.counts.peak.max(held.counts.bytes);
        held.parts.insert(self.key, part);
    }

    /// It is held never: the packing ahead is too far behind to pack it ([`HeldParts`]).
    pub fn skip(mut self) {
        self.done = true;
        let mut held = self.parts.held.lock().unwrap();
        if held.pending.remove(&self.key) {
            held.counts.skipped_parts += 1;
        }
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if !self.done {
            self.parts.held.lock().unwrap().pending.remove(&self.key);
        }
    }
}

#[cfg(any(test, feature = "testing"))]
impl HeldParts {
    /// For the tests of what governs it: a part taking `bytes` bytes held, until what this returns
    /// drops.
    pub fn hold_bytes(self: &Arc<Self>, bytes: usize) -> impl Send + use<> {
        let compressed = vec![0; bytes];
        let part = extent_frame::PackedPart { compressed: Some(compressed), crc: 0 };
        self.hold(Stored { part, raw: None })
    }
}

/// fxdax: a part held ([`HeldParts::hold`]): its segment's, which its framing takes, if it is still
/// held then ([`Hold::take`]). Dropped, the part is held no more.
pub(crate) struct Hold {
    key: u64,
    parts: Arc<HeldParts>,
}

impl Hold {
    /// The part, unless it was let go of to make room.
    pub fn take(self) -> Option<Stored> {
        self.parts.remove(self.key, true)
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        self.parts.remove(self.key, false);
    }
}

impl std::fmt::Debug for Hold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Hold({})", self.key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use extent_frame::PackedPart;

    /// A part packed into `len` bytes.
    fn part(len: usize) -> Stored {
        let compressed = vec![1; len];
        Stored { part: PackedPart { compressed: Some(compressed), crc: 0 }, raw: None }
    }

    /// TASK-400: parts held count their bytes until taken, dropped or let go of, those held last
    /// let go of first, down to the bytes asked for; a part let go of is taken as none.
    #[test]
    fn parts_go_newest_first() {
        let held = HeldParts::new();
        let holds: Vec<Hold> = [100, 200, 300, 400].map(|len| held.hold(part(len))).into();
        assert_eq!(held.bytes(), 1000);
        let mut holds = holds.into_iter();
        let (first, second, third, fourth) =
            (holds.next().unwrap(), holds.next().unwrap(), holds.next().unwrap(), holds.next());
        drop(fourth);
        assert_eq!(held.bytes(), 600, "dropped, held no more");
        assert_eq!(held.evict_to(250), 500, "the third, then the second");
        assert!(third.take().is_none() && second.take().is_none(), "let go of");
        let taken = first.take().expect("held still");
        assert_eq!(taken.bytes(), 100);
        let counts = held.counts();
        let expected = HeldCounts {
            bytes: 0,
            peak: 1000,
            held: 1000,
            taken: 100,
            evicted: 500,
            evicted_parts: 2,
            ..HeldCounts::default()
        };
        assert_eq!(counts, expected);
        assert_eq!(held.evict_to(0), 0, "none left");
    }

    /// TASK-400: a part pending, packed off the framer's thread, is held once filled, unless its
    /// framing took it first, counted late, and the part packed after is let go; a part dropped
    /// pending is wanted no more, and one skipped is counted, its framing finding none.
    #[test]
    fn parts_pending_are_held_once_packed_unless_taken() {
        let held = HeldParts::new();
        let (hold, pending) = held.pend();
        assert!(pending.wanted());
        pending.fill(part(100));
        assert_eq!(held.bytes(), 100);
        assert_eq!(hold.take().map(|part| part.bytes()), Some(100));

        let (late, pending) = held.pend();
        assert!(late.take().is_none(), "packed not yet");
        assert!(!pending.wanted(), "taken");
        pending.fill(part(200));
        assert_eq!(held.bytes(), 0, "packed after its framing took it, let go");

        let (dropped, pending) = held.pend();
        drop(dropped);
        assert!(!pending.wanted());

        let (skipped, pending) = held.pend();
        pending.skip();
        assert!(skipped.take().is_none());

        let (unfilled, pending) = held.pend();
        drop(pending);
        assert!(unfilled.take().is_none(), "never packed");
        let counts = held.counts();
        assert_eq!((counts.late_parts, counts.skipped_parts, counts.taken), (1, 1, 100));
    }
}
