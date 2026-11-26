// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What the agent keeps of its commands apart from their connections, so
//! that a host whose connection was reset can attach again: each output
//! stream's bytes from where the host may still need them, and the IDs of
//! commands it forgot, so that it never runs one of those again. No threads
//! or sockets in here, so every rule is tested alone.

use std::collections::{HashSet, VecDeque};

use libmachine_agent::{ENOBUFS, EPROTO, OUTPUT_KEPT, OUTPUT_UNSENT};

/// One output stream of a command (its stdout or stderr), by offset from
/// the command's start.
#[derive(Debug, Default)]
pub(crate) struct Stream {
    /// The bytes from `base` on.
    buf: VecDeque<u8>,
    /// The offset of `buf[0]`: what came before is no longer kept.
    base: u64,
    /// The furthest any connection has sent.
    sent: u64,
    /// The process's pipe has ended.
    pub(crate) eof: bool,
}

impl Stream {
    /// The offset after the last byte the process wrote.
    pub(crate) fn end(&self) -> u64 {
        self.base + self.buf.len() as u64
    }

    /// How much more the pump may read from the process now: reading stops
    /// [`OUTPUT_UNSENT`] ahead of what was sent, so that while no host takes
    /// the output the process waits instead of the output being lost.
    pub(crate) fn room(&self) -> usize {
        OUTPUT_UNSENT.saturating_sub((self.end() - self.sent) as usize)
    }

    /// What the process wrote.
    pub(crate) fn push(&mut self, data: &[u8]) {
        self.buf.extend(data);
    }

    /// Whether a host may send from `offset`: the errno to refuse it with if
    /// not (`ENOBUFS`: no longer kept; `EPROTO`: past what was written).
    pub(crate) fn check(&self, offset: u64) -> Result<(), i32> {
        if offset < self.base {
            Err(ENOBUFS)
        } else if offset > self.end() {
            Err(EPROTO)
        } else {
            Ok(())
        }
    }

    /// Up to `max` bytes from `offset` (which [`check`](Self::check) passed
    /// and nothing trimmed since: offsets a sender holds never fall behind
    /// what is kept, since it marks what it sends).
    pub(crate) fn chunk(&self, offset: u64, max: usize) -> Vec<u8> {
        let start = offset.saturating_sub(self.base) as usize;
        let n = (self.buf.len().saturating_sub(start)).min(max);
        self.buf.range(start..start + n).copied().collect()
    }

    /// A connection sent up to `offset`: what lies more than
    /// [`OUTPUT_KEPT`] before the furthest sent is dropped.
    pub(crate) fn mark_sent(&mut self, offset: u64) {
        self.sent = self.sent.max(offset.min(self.end()));
        let keep_from = self.sent.saturating_sub(OUTPUT_KEPT as u64);
        if keep_from > self.base {
            let n = (keep_from - self.base) as usize;
            self.buf.drain(..n);
            self.base = keep_from;
        }
    }

    /// Nobody will take the output again (the command was released): keep
    /// nothing, so that a pump reading on to the end holds no memory.
    pub(crate) fn discard(&mut self) {
        self.base = self.end();
        self.sent = self.base;
        self.buf.clear();
    }

    /// The bytes kept.
    pub(crate) fn kept(&self) -> usize {
        self.buf.len()
    }
}

/// The IDs of commands the agent forgot (their results expired, or were
/// evicted to make room) while a host might still ask for them: an `Exec`
/// of one of them must not run it again. Bounded: the oldest go first.
#[derive(Debug)]
pub(crate) struct Forgotten {
    order: VecDeque<u64>,
    set: HashSet<u64>,
    capacity: usize,
}

impl Forgotten {
    pub(crate) fn new(capacity: usize) -> Self {
        Self { order: VecDeque::new(), set: HashSet::new(), capacity }
    }

    pub(crate) fn insert(&mut self, id: u64) {
        if self.capacity == 0 || !self.set.insert(id) {
            return;
        }
        self.order.push_back(id);
        if self.order.len() > self.capacity {
            let old = self.order.pop_front().expect("not empty");
            self.set.remove(&old);
        }
    }

    pub(crate) fn contains(&self, id: u64) -> bool {
        self.set.contains(&id)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.order.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_is_read_ahead_only_so_far_and_kept_behind_what_was_sent() {
        let mut s = Stream::default();
        assert_eq!(s.room(), OUTPUT_UNSENT);
        s.push(&vec![1; OUTPUT_UNSENT]);
        assert_eq!(s.room(), 0, "nothing sent: the process waits");
        assert_eq!(s.chunk(0, 10), vec![1; 10]);
        s.mark_sent(10);
        assert_eq!(s.room(), 10);
        // Sending on keeps OUTPUT_KEPT behind the furthest sent.
        let mut written = OUTPUT_UNSENT as u64;
        while written < 4 * OUTPUT_KEPT as u64 {
            let n = s.room();
            s.push(&vec![2; n]);
            written += n as u64;
            s.mark_sent(written);
        }
        assert_eq!(s.end(), written);
        assert_eq!(s.kept(), OUTPUT_KEPT);
        assert_eq!(s.check(written - OUTPUT_KEPT as u64), Ok(()));
        assert_eq!(s.check(written - OUTPUT_KEPT as u64 - 1), Err(ENOBUFS));
        assert_eq!(s.check(written), Ok(()), "at the end: nothing more yet");
        assert_eq!(s.check(written + 1), Err(EPROTO));
    }

    #[test]
    fn a_host_attaching_again_gets_what_it_missed_in_order() {
        let mut s = Stream::default();
        let data: Vec<u8> = (0..300_000u32).map(|i| i as u8).collect();
        s.push(&data[..200_000]);
        // The first connection sent 150 000 bytes, the host got 100 000.
        s.mark_sent(150_000);
        s.push(&data[200_000..]);
        assert_eq!(s.check(100_000), Ok(()));
        let mut got = data[..100_000].to_vec();
        let mut at = 100_000;
        while at < s.end() {
            let c = s.chunk(at, 7_000);
            at += c.len() as u64;
            got.extend(c);
            s.mark_sent(at);
        }
        assert_eq!(got, data);
        // Marks behind the furthest never move it back, nor past the end.
        s.mark_sent(5);
        s.mark_sent(u64::MAX);
        assert_eq!(s.room(), OUTPUT_UNSENT);
    }

    #[test]
    fn discarded_output_holds_nothing() {
        let mut s = Stream::default();
        s.push(&[0; 1000]);
        s.discard();
        assert_eq!((s.kept(), s.end(), s.room()), (0, 1000, OUTPUT_UNSENT));
    }

    #[test]
    fn forgotten_ids_are_bounded_oldest_first() {
        let mut f = Forgotten::new(3);
        for id in [1, 2, 2, 3, 4] {
            f.insert(id);
        }
        assert_eq!(f.len(), 3);
        assert!(!f.contains(1));
        assert!(f.contains(2) && f.contains(3) && f.contains(4));
        Forgotten::new(0).insert(1);
    }
}
