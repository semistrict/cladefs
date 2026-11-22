// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: reading bytes from outside the process: the host's disk read back, a peer's replica
//! logs. Each read is checked, so that no bytes, however they were made, panic or wrap what reads
//! them; a reader of such bytes reads them through [`Cursor`] and denies clippy's
//! `arithmetic_side_effects` and `indexing_slicing`, as this module does (AGENTS.md, Trust
//! boundaries; board 5408, 5436).

#![deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]

/// Bytes read from their start, each read checked: `None` once a read wants more than are left,
/// which then are left as they were.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }

    /// The next `len` bytes.
    pub(crate) fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let (taken, rest) = self.0.split_at_checked(len)?;
        self.0 = rest;
        Some(taken)
    }

    /// The next `N` bytes.
    pub(crate) fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (taken, rest) = self.0.split_first_chunk::<N>()?;
        self.0 = rest;
        Some(*taken)
    }

    /// The next little-endian `u64`.
    pub(crate) fn u64(&mut self) -> Option<u64> {
        self.array().map(u64::from_le_bytes)
    }

    /// The next little-endian `u32`.
    pub(crate) fn u32(&mut self) -> Option<u32> {
        self.array().map(u32::from_le_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A read past the end is none, and leaves what is left.
    #[test]
    fn reads_past_the_end_are_none() {
        let mut bytes = Cursor::new(&[1, 0, 0, 0, 0, 0, 0, 0, 9]);
        assert_eq!(bytes.u64(), Some(1));
        assert_eq!(bytes.u64(), None);
        assert_eq!(bytes.take(usize::MAX), None);
        assert_eq!(bytes.take(1), Some(&[9][..]));
        assert_eq!(bytes.u32(), None);
    }
}
