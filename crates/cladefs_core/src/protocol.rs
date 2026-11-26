// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Canonical protocol limits, shared doc-4 wire flag bits, and checked validation helpers shared
//! between the virtio extent-store device (`virtio_extent`) and the extent daemon's wire protocol
//! (`cladefsd::wire` / `cladefsd::codec`).
//!
//! Sharing these definitions prevents the two protocols from drifting while each trust boundary
//! still decodes and validates untrusted input independently (AGENTS.md, board 5702): the VM
//! process does not trust the guest, and the extent daemon does not trust a compromised VM
//! process.

use crate::{Commit, Errno, ExtentId, MappingContents, OpenCell, WindowMapping, WriteBarriers};

/// The most bytes one extent holds.
pub const MAX_EXTENT: u64 = OpenCell::MAX_EXTENT;

/// The most bytes one virtio `READ` or `WRITE` moves.
pub const MAX_TRANSFER: u32 = 8 << 20;

/// The most bytes a cell's root holds (`COMMIT_ROOT`, `WRITE_ROOT`).
pub const MAX_ROOT: usize = storage_device::MAX_ROOT;

/// The most bytes of guest stats text a `STATS` request carries (TASK-111.4).
pub const MAX_STATS: usize = 64 << 10;

/// The most bytes of extra entries a virtio `FREE_MANY` or `MAP_MANY` carries (one largest guest
/// page).
pub const MAX_MANY_DATA: usize = 64 << 10;

/// The most bytes a VM process's DAX window has (TASK-150.3).
pub const MAX_WINDOW: u64 = 64 << 30;

// Shared doc-4 wire flags: the daemon's wire uses virtio's exact bit positions for every flag
// both protocols share (TASK-474).

/// A `WRITE` preceded by an ordering barrier.
pub const WRITE_BARRIER: u32 = 1 << 0;
/// A `WRITE` followed by a commit.
pub const WRITE_COMMIT: u32 = 1 << 1;
/// A `WRITE`'s commit completes once uploaded.
pub const WRITE_UPLOADED: u32 = 1 << 2;
/// A `WRITE`'s commit sets the root.
pub const WRITE_ROOT: u32 = 1 << 3;
/// All doc-4 `WRITE` flag bits shared by virtio and `cladefsd`.
pub const WRITE_SHARED_FLAGS: u32 = WRITE_BARRIER | WRITE_COMMIT | WRITE_UPLOADED | WRITE_ROOT;

/// A `COMMIT` completes once uploaded.
pub const COMMIT_UPLOADED: u32 = 1 << 0;
/// A `COMMIT` sets the root.
pub const COMMIT_ROOT: u32 = 1 << 1;
/// All doc-4 `COMMIT` flag bits shared by virtio and `cladefsd`.
pub const COMMIT_SHARED_FLAGS: u32 = COMMIT_UPLOADED | COMMIT_ROOT;

/// A `MAP` mapping zeros rather than an extent's bytes (doc-4 bit 3; bit 0 was retired writable
/// DAX).
pub const MAP_ZEROS: u32 = 1 << 3;

/// Validates an extent ID from untrusted input: refuses (`EINVAL`) `sequence == u64::MAX`, past
/// which no cell's next sequence or spent run's end exists (board 2762).
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub fn check_extent_id(cell: u64, sequence: u64) -> Result<ExtentId, Errno> {
    match sequence {
        u64::MAX => Err(Errno::EINVAL),
        sequence => Ok(ExtentId { cell, sequence }),
    }
}

/// Refuses (`EINVAL`) a range `[offset, offset + len)` that overflows `u64` (board 2776).
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub fn check_range_ends(offset: u64, len: u64) -> Result<(), Errno> {
    offset.checked_add(len).map(drop).ok_or(Errno::EINVAL)
}

/// Refuses (`EINVAL`) unless `alignment` is non-zero, `offset` and `len` are multiples of
/// `alignment`, and `offset + len <= most` without wrapping (TASK-470).
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub fn check_range_within(offset: u64, len: u64, alignment: u64, most: u64) -> Result<(), Errno> {
    if alignment == 0 {
        return Err(Errno::EINVAL);
    }
    let aligned = offset.is_multiple_of(alignment) && len.is_multiple_of(alignment);
    match offset.checked_add(len) {
        Some(end) if aligned && end <= most => Ok(()),
        _ => Err(Errno::EINVAL),
    }
}

/// Refuses (`EINVAL`) a window range `[offset, offset + len)` that overflows `u64` or ends past
/// [`MAX_WINDOW`] (board 2776).
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub fn check_window_range(offset: u64, len: u64) -> Result<(), Errno> {
    match offset.checked_add(len) {
        Some(end) if end <= MAX_WINDOW => Ok(()),
        _ => Err(Errno::EINVAL),
    }
}

/// Validates a [`WindowMapping`]'s window range (within [`MAX_WINDOW`]), extent ID (not the last
/// sequence), and extent byte range (within `u64`).
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub fn check_mapping(mapping: WindowMapping) -> Result<WindowMapping, Errno> {
    check_window_range(mapping.window_offset, mapping.len)?;
    if let MappingContents::Extent { extent, offset } = mapping.contents {
        check_extent_id(extent.cell, extent.sequence)?;
        check_range_ends(offset, mapping.len)?;
    }
    Ok(mapping)
}

/// Refuses (`EINVAL`) a root longer than [`MAX_ROOT`] bytes (TASK-149.17).
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub fn check_root_len(len: usize) -> Result<(), Errno> {
    (len <= MAX_ROOT).then_some(()).ok_or(Errno::EINVAL)
}

/// Validates guest stats text: refuses (`EINVAL`) payloads longer than [`MAX_STATS`] bytes or
/// invalid UTF-8 (TASK-111.4).
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub fn check_stats_text(payload: &[u8]) -> Result<&str, Errno> {
    if payload.len() > MAX_STATS {
        return Err(Errno::EINVAL);
    }
    std::str::from_utf8(payload).map_err(|_| Errno::EINVAL)
}

/// Decodes a `WRITE`'s barrier/commit flags (excluding transport-specific bits such as
/// `WRITE_TRANSFER`), optionally allowing a `local_bit` for `Commit::Local`. Returns the
/// [`WriteBarriers`] (with `root: None`) and whether [`WRITE_ROOT`] is set.
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub fn decode_write_barriers(
    flags: u32,
    local_bit: Option<u32>,
) -> Result<(WriteBarriers<'static>, bool), Errno> {
    let local_mask = local_bit.unwrap_or(0);
    let known = WRITE_SHARED_FLAGS | local_mask;
    if flags & !known != 0 {
        return Err(Errno::EINVAL);
    }
    let has = |bit: u32| flags & bit != 0;
    let commit = has(WRITE_COMMIT);
    let uploaded = has(WRITE_UPLOADED);
    let local = local_bit.is_some_and(has);
    let has_root = has(WRITE_ROOT);
    // Commit-dependent flags without WRITE_COMMIT, or both uploaded and local at once.
    if (!commit && (uploaded || local || has_root)) || (uploaded && local) {
        return Err(Errno::EINVAL);
    }
    let after = commit.then_some(match (uploaded, local) {
        (true, false) => Commit::Uploaded,
        (false, true) => Commit::Local,
        (false, false) => Commit::Ordered,
        (true, true) => unreachable!("checked above"),
    });
    let barriers = WriteBarriers { before: has(WRITE_BARRIER), after, root: None };
    Ok((barriers, has_root))
}

/// Decodes a `COMMIT`'s flags, optionally allowing a `local_bit` for `Commit::Local`. Returns the
/// [`Commit`] level and whether [`COMMIT_ROOT`] is set.
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub fn decode_commit_flags(flags: u32, local_bit: Option<u32>) -> Result<(Commit, bool), Errno> {
    let local_mask = local_bit.unwrap_or(0);
    let known = COMMIT_SHARED_FLAGS | local_mask;
    if flags & !known != 0 {
        return Err(Errno::EINVAL);
    }
    let has = |bit: u32| flags & bit != 0;
    let uploaded = has(COMMIT_UPLOADED);
    let local = local_bit.is_some_and(has);
    let commit = match (uploaded, local) {
        (true, true) => return Err(Errno::EINVAL),
        (true, false) => Commit::Uploaded,
        (false, true) => Commit::Local,
        (false, false) => Commit::Ordered,
    };
    Ok((commit, has(COMMIT_ROOT)))
}
