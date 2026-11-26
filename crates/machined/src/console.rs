// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A machine's console: the end of what its guest wrote to its UART, kept across starts, each byte
//! at its offset in all the guest ever wrote, so that readers (StreamConsole) can follow it from
//! where they are and see what they missed. A writer never waits for a reader: what no reader
//! took in time is dropped from the front.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::watch;

/// How much of the console a machine keeps: its end.
pub(crate) const KEPT: usize = 1 << 20;

#[derive(Debug)]
pub(crate) struct Console {
    kept: Mutex<Kept>,
    /// The offset of the end of what was written: readers wait on it.
    end: watch::Sender<u64>,
}

#[derive(Debug, Default)]
struct Kept {
    bytes: VecDeque<u8>,
    /// The offset of `bytes`' first.
    start: u64,
}

fn lock(m: &Mutex<Kept>) -> MutexGuard<'_, Kept> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Console {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self { kept: Mutex::default(), end: watch::channel(0).0 })
    }

    /// What the guest's UART writes to: the machine's console sink.
    pub(crate) fn writer(self: &Arc<Self>) -> impl Write + Send + 'static {
        Writer(self.clone())
    }

    fn append(&self, data: &[u8]) {
        let mut kept = lock(&self.kept);
        kept.bytes.extend(data);
        let over = kept.bytes.len().saturating_sub(KEPT);
        kept.bytes.drain(..over);
        kept.start = kept.start.saturating_add(over as u64);
        let end = kept.start.saturating_add(kept.bytes.len() as u64);
        drop(kept);
        self.end.send_replace(end);
    }

    /// At most `max` bytes from `offset`, or from the first byte kept if `offset` is no longer
    /// kept: where they start, and them (none at the end).
    pub(crate) fn read(&self, offset: u64, max: usize) -> (u64, Vec<u8>) {
        let kept = lock(&self.kept);
        let from = offset.max(kept.start);
        // `from - start` is within what is kept, or past its end (nothing to read).
        let skip = usize::try_from(from.saturating_sub(kept.start)).unwrap_or(usize::MAX);
        let data: Vec<u8> = kept.bytes.iter().skip(skip).take(max).copied().collect();
        (from, data)
    }

    /// Changes to where what was written ends.
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.end.subscribe()
    }
}

struct Writer(Arc<Console>);

impl Write for Writer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.append(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader behind what is kept starts at the first byte kept, and knows it from the offset.
    #[test]
    fn a_reader_behind_skips_to_what_is_kept() {
        let console = Console::new();
        let mut w = console.writer();
        w.write_all(&vec![b'a'; KEPT]).unwrap();
        w.write_all(b"xyz").unwrap();
        assert_eq!(*console.subscribe().borrow(), KEPT as u64 + 3);
        let (from, data) = console.read(0, 2);
        assert_eq!((from, data), (3, b"aa".to_vec()));
        let (from, data) = console.read(KEPT as u64, 10);
        assert_eq!((from, data), (KEPT as u64, b"xyz".to_vec()));
        assert_eq!(console.read(KEPT as u64 + 3, 10), (KEPT as u64 + 3, Vec::new()));
        assert_eq!(console.read(u64::MAX, 10), (u64::MAX, Vec::new()));
    }
}
