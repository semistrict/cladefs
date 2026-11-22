// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the guest tests' known plaintext (TASK-411.15): bytes that don't compress, the same in
//! every build of the host's half and the guest's, which cases write into their guests'
//! filesystems (fxfs-guest's file, fxfs-guest-checks' mapped file), and the harness looks for where
//! cells keep data, their directories, their buckets and part caches (`crate::encrypted`): in a run
//! whose cells are encrypted, it finds none. Not compressible, so that a cell in the clear keeps it
//! as written in its bucket's objects too, whose parts are compressed where that pays, not only in
//! its raw files.

use std::sync::OnceLock;

/// How many of the stream's bytes the harness looks for: its first [`LEN`].
pub const LEN: usize = 1 << 20;

/// The stream's first `len` bytes: xorshift64*'s, from a seed of its own. No 4 KiB block of it
/// repeats another.
pub fn stream(len: usize) -> Vec<u8> {
    let mut state: u64 = 0x6678_6461_7863_616e; // "cladefscan"
    let mut bytes = Vec::with_capacity(len.next_multiple_of(8));
    while bytes.len() < len {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        bytes.extend_from_slice(&state.wrapping_mul(0x2545_f491_4f6c_dd1d).to_le_bytes());
    }
    bytes.truncate(len);
    bytes
}

/// The stream's first [`LEN`] bytes, what the harness looks for.
pub fn bytes() -> &'static [u8] {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    BYTES.get_or_init(|| stream(LEN))
}

/// The byte at `offset` of the stream's first [`LEN`] bytes, repeated: content of a file at
/// `offset`, which a reader checks byte by byte.
pub fn at(offset: usize) -> u8 {
    bytes()[offset % LEN]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stream is the same in every build: its first bytes as they were first drawn. And it
    /// doesn't compress, nor repeat a block.
    #[test]
    fn the_stream_is_fixed_and_incompressible() {
        assert_eq!(&bytes()[..8], &stream(8)[..]);
        assert_eq!(stream(8), [0xe5, 0x0a, 0x38, 0xf7, 0xbd, 0xb3, 0xa3, 0x35]);
        assert_eq!(stream(LEN + 3)[..LEN], *bytes());
        assert_eq!(at(LEN + 5), bytes()[5]);
        let blocks: std::collections::HashSet<&[u8]> = bytes().chunks(4096).collect();
        assert_eq!(blocks.len(), LEN / 4096);
        let distinct: std::collections::HashSet<u8> = bytes()[..4096].iter().copied().collect();
        assert!(distinct.len() > 250, "{}", distinct.len());
    }
}
