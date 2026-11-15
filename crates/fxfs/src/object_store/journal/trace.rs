// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: traces of the journal's steps that specs/journal's actions are of, for TLC to check that
//! a run of the code is a behaviour of the spec (specs/journal/trace, TASK-444.1), as the
//! followers' are (crates/follower/src/shuttle_trace.rs). Compiled with the `journal_trace`
//! feature only, and recorded only for a journal given a [`JournalTrace`]
//! (`JournalOptions::trace`).
//!
//! Each step is recorded where the code makes it, under the lock it makes it under, with what it
//! saw, in the journal's own terms (byte offsets, object IDs): the test that drives the code maps
//! them to the spec's. A transaction, under the journal's writer lock, once its Commit record is
//! written and its mutations applied; write_super_block's pad, flush and save; the journal durable
//! further; a replay's result, at a mount.

use fuchsia_sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// An object's offsets in the journal: of the mutations its flush under way writes, and of those
/// since (`ObjectManager`'s `Checkpoints`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObjectOffsets {
    pub object_id: u64,
    pub old: Option<u64>,
    pub cur: Option<u64>,
}

/// A step of the journal's, as the code made it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    /// A transaction written from `start` to `end`, mutating the objects `mutated`, beginning the
    /// flush of `begin` or ending that of `end_flush`, after which the objects' offsets are
    /// `offsets`. The root parent store's mutations, whose store the super-block holds, and the
    /// journal's own, are none of these.
    Commit {
        start: u64,
        end: u64,
        mutated: Vec<u64>,
        begin: Option<u64>,
        end_flush: Option<u64>,
        offsets: Vec<ObjectOffsets>,
    },
    /// write_super_block padded the journal, its checkpoint `checkpoint`.
    Pad { checkpoint: u64 },
    /// It flushed the device, the journal durable as far as `durable`.
    Flush { durable: u64 },
    /// It saved a super-block whose journal goes on at `checkpoint`, and whose objects' offsets are
    /// `offsets`.
    Save { checkpoint: u64, offsets: Vec<(u64, u64)> },
    /// The journal is durable as far as `durable`: a flush of the device, or a barrier after its
    /// write, made it so.
    Durable { durable: u64 },
    /// A mount replayed the journal from a super-block whose journal goes on at `checkpoint`, and
    /// whose objects' offsets are `offsets`, applying the transactions that end at `end` at the
    /// latest, after which the objects' offsets are `offsets_after`.
    Replayed {
        checkpoint: u64,
        offsets: Vec<(u64, u64)>,
        end: u64,
        offsets_after: Vec<ObjectOffsets>,
    },
    /// The power is cut here, as the test that drives the code says ([`JournalTrace::cut`]).
    Cut,
    /// The code did what the spec has no action for, which fails the trace.
    Unsupported(String),
}

/// A journal's trace, while it records.
#[derive(Default)]
pub struct JournalTrace {
    events: Mutex<Vec<Event>>,
    /// Whether write_super_block reads the objects' offsets after its flush, as before
    /// TASK-149.5's fix: a mutant whose traces must not be the spec's.
    offsets_after_flush: AtomicBool,
}

impl JournalTrace {
    pub fn new() -> Self {
        Self::default()
    }

    /// The trace of a mutant, whose write_super_block reads the objects' offsets after its flush.
    pub fn of_offsets_read_after_flush() -> Self {
        Self { offsets_after_flush: AtomicBool::new(true), ..Self::default() }
    }

    pub(crate) fn offsets_after_flush(&self) -> bool {
        self.offsets_after_flush.load(Ordering::Relaxed)
    }

    pub(crate) fn push(&self, event: Event) {
        self.events.lock().push(event);
    }

    /// The power is cut here: returns the cut's index, among the cuts of this trace.
    pub fn cut(&self) -> usize {
        let mut events = self.events.lock();
        let index = events.iter().filter(|event| matches!(event, Event::Cut)).count();
        events.push(Event::Cut);
        index
    }

    /// The steps recorded so far, in order.
    pub fn events(&self) -> Vec<Event> {
        self.events.lock().clone()
    }
}
