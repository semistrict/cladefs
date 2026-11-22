// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: what a power loss leaves of a write made since its file's last sync, as both crash
//! models have it: each simulated host's disk (`sim`, under madsim) and the cells' own crash tests'
//! (`local_cell::MemDisk`) (board 5748, as FoundationDB's `AsyncFileNonDurable` has it).
//!
//! A write lands in sectors of [`SECTOR`] bytes at the file's offsets, as a drive writes them, a
//! write's first and last sectors partial if it is unaligned. Each sector is kept or not, at
//! random; and, corrupting, also garbage or torn: cut at a random offset, its left part or its
//! right part written, the rest as it was or garbage. Garbage is only ever in the bytes the write
//! changes: a crash damages no byte a write leaves as it is, the sector's others included
//! (powersafe overwrite, which cladefs assumes of its disks as SQLite does of Linux's by default:
//! board 5890, decision-3).

/// The bytes a drive writes whole or not, at the file's offsets that are multiples of it: a
/// crash keeps each sector of a write, or not, or, corrupting, tears it.
pub const SECTOR: usize = 512;

/// Applies to `data`, a file's bytes as a crash leaves them so far, what the crash leaves of
/// `bytes`, written at `at` since the file's last sync: each sector of it kept, or not, and, if
/// `corrupting`, garbage or torn too ([`self`]). `draw(n)` draws a number below `n`: the crash's
/// seed's.
pub fn write(
    data: &mut Vec<u8>,
    at: usize,
    bytes: &[u8],
    corrupting: bool,
    draw: &mut dyn FnMut(u64) -> u64,
) {
    let mut start = at;
    while start < at + bytes.len() {
        let end = ((start / SECTOR + 1) * SECTOR).min(at + bytes.len());
        let written = &bytes[start - at..end - at];
        let fate = draw(if corrupting { 4 } else { 2 });
        if fate != 0 {
            if data.len() < end {
                data.resize(end, 0);
            }
            let on = &mut data[start..end];
            match fate {
                1 => on.copy_from_slice(written),
                2 => garbled(on, written, draw),
                _ => torn(on, written, draw),
            }
        }
        start = end;
    }
}

/// `on`, the disk's bytes of a sector a crash cut `written` short in, torn: cut at a random
/// offset, its left part or its right part written, the other as it was or garbage.
fn torn(on: &mut [u8], written: &[u8], draw: &mut dyn FnMut(u64) -> u64) {
    if on.len() < 2 {
        return garbled(on, written, draw);
    }
    let cut = 1 + draw(on.len() as u64 - 1) as usize;
    let (left, right) = on.split_at_mut(cut);
    let (left_written, right_written) = written.split_at(cut);
    let (new, new_written, other, other_written) = match draw(2) {
        0 => (left, left_written, right, right_written),
        _ => (right, right_written, left, left_written),
    };
    new.copy_from_slice(new_written);
    if draw(2) == 1 {
        garbled(other, other_written, draw);
    }
}

/// `on`, the disk's bytes a crash cut `written` short over, as garbage: those `written` changes,
/// at random; those it writes as they were, as they were.
pub fn garbled(on: &mut [u8], written: &[u8], draw: &mut dyn FnMut(u64) -> u64) {
    for (byte, &new) in on.iter_mut().zip(written) {
        if *byte != new {
            *byte = draw(256) as u8;
        }
    }
}
