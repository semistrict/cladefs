// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: why a commit's sealing copies the extents it copies, a debug stat (TASK-92.41): with
//! CLADEFS_SEAL_STATS set in its environment, each commit that copies anything says, by why, how
//! many extents it copied and how many bytes, logged; otherwise nothing is counted. A commit seals
//! each extent open in one run in the open segment, or one filled since the last commit, or a file
//! of its own; one whose data is not one run there already, it copies whole (see `LocalCell::seal`),
//! which the commit's flush then syncs: up to 111 MiB at once under pgbench, the turns after
//! waiting for it (GCE r958, TASK-221).

use crate::layout::{Piece, SegmentId};

/// Where a segment stands for a commit sealing the extents in it: its [`Phase`](crate::segments),
/// as the sealing sees it, of no data; a failed framing's as framing, and every segment neither
/// open, full nor framing as closed (`Segments::standing`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Standing {
    /// The open segment.
    Open,
    /// Filled since the last commit, waiting for the next, which seals in it (TASK-92.41).
    Full,
    /// Closed since the last commit, being framed: by the uploader.
    Framing,
    /// Closed and framed, as of earlier commits.
    Closed,
}

/// Why a sealed extent was copied, or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Why {
    /// One run in the open segment already, or in one filled since the last commit: not copied.
    OneRun,
    /// Written to a raw file of its own, framed from it into its own file.
    Own,
    /// In pieces, `large_extent` or more: copied into a file of its own.
    Large,
    /// Some of its data in segments closed and framed, as of earlier commits: written over, or
    /// grown, since it was last sealed. `new` of its bytes are in the open segment.
    Committed { new: u64 },
    /// In more than one of the open segment and those filled since the last commit: written as
    /// one filled.
    Spanning,
    /// Some of its data in segments closed since the last commit, being framed.
    Framing,
    /// All in one segment, the open one or one filled, but not all of it: written with holes, or
    /// not from its start.
    Holes,
    /// All in one segment, in order, but other writes between its pieces: interleaved with other
    /// extents' writes.
    Interleaved,
    /// All in one segment, but its pieces out of order there: parts written over, or written out
    /// of order, since the last commit.
    Rewritten,
    /// All in one segment, its pieces one after the other there, but not joined.
    Adjacent,
}

impl Why {
    const NAMES: [&str; 10] = [
        "one_run",
        "own",
        "large",
        "committed",
        "holes",
        "interleaved",
        "rewritten",
        "adjacent",
        "spanning",
        "framing",
    ];

    fn index(self) -> usize {
        match self {
            Why::OneRun => 0,
            Why::Own => 1,
            Why::Large => 2,
            Why::Committed { .. } => 3,
            Why::Holes => 4,
            Why::Interleaved => 5,
            Why::Rewritten => 6,
            Why::Adjacent => 7,
            Why::Spanning => 8,
            Why::Framing => 9,
        }
    }

    /// Why an extent of `len` bytes, whose data is in `pieces`, is copied as it is sealed, or
    /// not, each segment where `standing` says; `large` if `large_extent` or more.
    pub fn of_pieces(
        pieces: &[Piece],
        len: u64,
        standing: impl Fn(SegmentId) -> Standing,
        large: bool,
    ) -> Self {
        if large {
            return Why::Large;
        }
        let standings: Vec<Standing> = pieces.iter().map(|piece| standing(piece.segment)).collect();
        if standings.contains(&Standing::Closed) {
            let new = (pieces.iter().zip(&standings))
                .filter(|(_, standing)| **standing == Standing::Open)
                .map(|(piece, _)| piece.len);
            return Why::Committed { new: new.sum() };
        }
        if standings.contains(&Standing::Framing) {
            return Why::Framing;
        }
        if pieces.iter().any(|piece| piece.segment != pieces[0].segment) {
            return Why::Spanning;
        }
        let mut by_offset: Vec<&Piece> = pieces.iter().collect();
        by_offset.sort_by_key(|piece| piece.extent_offset);
        let mut end = 0;
        for piece in &by_offset {
            if piece.extent_offset != end {
                return Why::Holes;
            }
            end += piece.len;
        }
        if end != len {
            return Why::Holes;
        }
        if let [piece] = by_offset[..] {
            debug_assert!(piece.extent_offset == 0 && piece.len == len);
            return Why::OneRun;
        }
        let pairs = || by_offset.windows(2).map(|pair| (pair[0], pair[1]));
        if pairs().all(|(a, b)| b.offset == a.offset + a.len) {
            return Why::Adjacent;
        }
        if pairs().all(|(a, b)| b.offset >= a.offset + a.len) {
            return Why::Interleaved;
        }
        Why::Rewritten
    }
}

/// Whether CLADEFS_SEAL_STATS is set: read once.
pub(crate) fn enabled() -> bool {
    static ENABLED: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var_os("CLADEFS_SEAL_STATS").is_some());
    *ENABLED
}

/// A commit's sealing, as [`Why`] each extent was copied: how many, how many bytes, how many of
/// them new for [`Why::Committed`], and the most pieces of one copied.
#[derive(Default)]
pub(crate) struct Sealing {
    count: [u64; 10],
    bytes: [u64; 10],
    committed_new: u64,
    most_pieces: usize,
}

impl Sealing {
    /// An extent of `len` bytes in `pieces` pieces was sealed, for `why`.
    pub fn sealed(&mut self, why: Why, len: u64, pieces: usize) {
        self.count[why.index()] += 1;
        self.bytes[why.index()] += len;
        if let Why::Committed { new } = why {
            self.committed_new += new;
        }
        if why != Why::OneRun {
            self.most_pieces = self.most_pieces.max(pieces);
        }
    }

    /// Logs it, the commit of `epoch` of cell `cell`'s, if it copied anything.
    pub fn log(&self, cell: u64, epoch: u64) {
        let copied: u64 = self.bytes[1..].iter().sum();
        if copied == 0 {
            return;
        }
        let whys: Vec<String> = (Why::NAMES.iter().enumerate())
            .filter(|(why, _)| self.count[*why] > 0)
            .map(|(why, name)| format!("{name} {} {} KiB", self.count[why], self.bytes[why] >> 10))
            .collect();
        log::info!(
            "extent store: seal stats: cell {} commit {epoch}: copied {} KiB; {}; \
             committed new {} KiB; most pieces {}",
            base32_id::Short(cell),
            copied >> 10,
            whys.join(", "),
            self.committed_new >> 10,
            self.most_pieces
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPEN: SegmentId = SegmentId { cell: 1, number: 5 };
    const OLD: SegmentId = SegmentId { cell: 1, number: 4 };

    fn piece(segment: SegmentId, offset: u64, extent_offset: u64, len: u64) -> Piece {
        Piece { segment, offset, extent_offset, len }
    }

    /// Each why, from the pieces that make it.
    fn standing(segment: SegmentId) -> Standing {
        match segment.number {
            5 => Standing::Open,
            3 => Standing::Full,
            2 => Standing::Framing,
            _ => Standing::Closed,
        }
    }

    #[test]
    fn pieces_say_why() {
        let why = |pieces: &[Piece], len| Why::of_pieces(pieces, len, standing, false);
        assert_eq!(why(&[piece(OPEN, 0, 0, 8)], 8), Why::OneRun);
        assert_eq!(why(&[piece(OLD, 0, 0, 4), piece(OPEN, 0, 4, 4)], 8), Why::Committed { new: 4 });
        assert_eq!(why(&[piece(OPEN, 0, 4, 4)], 8), Why::Holes);
        assert_eq!(why(&[piece(OPEN, 0, 0, 4), piece(OPEN, 9, 4, 4)], 8), Why::Interleaved);
        assert_eq!(why(&[piece(OPEN, 9, 0, 4), piece(OPEN, 0, 4, 4)], 8), Why::Rewritten);
        assert_eq!(why(&[piece(OPEN, 0, 0, 4), piece(OPEN, 4, 4, 4)], 8), Why::Adjacent);
        assert_eq!(Why::of_pieces(&[piece(OPEN, 0, 0, 8)], 8, standing, true), Why::Large);
        let full = SegmentId { cell: 1, number: 3 };
        let framing = SegmentId { cell: 1, number: 2 };
        assert_eq!(why(&[piece(full, 0, 0, 4), piece(OPEN, 0, 4, 4)], 8), Why::Spanning);
        assert_eq!(why(&[piece(full, 0, 0, 8)], 8), Why::OneRun);
        assert_eq!(why(&[piece(full, 0, 0, 4), piece(full, 9, 4, 4)], 8), Why::Interleaved);
        assert_eq!(why(&[piece(framing, 0, 0, 8)], 8), Why::Framing);
    }
}
