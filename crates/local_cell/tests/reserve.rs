// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-539.7: what a cell's own work, never refused, adds to its directory for what its guest
//! wrote is within what its footprint reserves for it (`local_cell::footprint`, backlog doc-22): at
//! any point of a cell's life, its guest's writes stopped there, its commits, closings, framings
//! and flushes, run to their end, take its directory past what it took by no more than its
//! reserve. And a disk the admission weighs writes against, as full as it lets it be, refuses only
//! guests' writes: its cells' own work always finds room.

mod common;

use common::noise;
use local_cell::footprint::DiskFull;
use local_cell::{Description, LocalCell, MemDir, Options};
use rand::rngs::StdRng;
use rand::{RngExt as _, SeedableRng as _};
use std::collections::BTreeMap;
use storage_device::ExtentId;

const BLOCK: u64 = 4096;
const CELL: bucket_cell::layout::CellId = bucket_cell::layout::test_cell(7);

/// Small segments and large extents, as the cell's tests have them, so that a workload makes
/// several of each: framed in place and in copies, own extents, commits copying pieces, held runs.
const OPTIONS: Options = Options {
    max_extent: 64 << 20,
    segment_size: 64 << 10,
    segment_age: std::time::Duration::from_secs(3600),
    large_extent: 32 << 10,
    open_files: 2,
    held_most: 256 << 10,
};

/// `len` bytes at random: noise, which no framing compresses, or a pattern, which compresses and
/// is framed in a copy.
fn data(rng: &mut StdRng, len: u64) -> Vec<u8> {
    match rng.random() {
        true => noise(rng.random(), len as usize),
        false => (0..len).map(|at| (at % 7) as u8).collect(),
    }
}

/// A cell's guest, at random: the extents it has open, each as far as it wrote it, and those
/// sealed.
struct Guest {
    rng: StdRng,
    next: u64,
    open: BTreeMap<u64, u64>,
    sealed: Vec<u64>,
}

impl Guest {
    fn new(seed: u64) -> Self {
        Self {
            rng: StdRng::seed_from_u64(seed),
            next: 1,
            open: BTreeMap::new(),
            sealed: Vec::new(),
        }
    }

    /// Does a guest's next thing to `cell`: writes small and large, sparse or not, to a new extent
    /// or one open; commits; frees; closes the open segment. Returns what a write was refused by.
    async fn step(&mut self, cell: &LocalCell<MemDir>) -> Option<DiskFull> {
        let rng = &mut self.rng;
        match rng.random_range(0..10) {
            0..6 => {
                let sequence = match self.open.is_empty() || rng.random() {
                    true => (self.next, self.next += 1).0,
                    false => *self.open.keys().nth(rng.random_range(0..self.open.len())).unwrap(),
                };
                let blocks = match rng.random_range(0..4) {
                    0 => rng.random_range(8..24),
                    _ => rng.random_range(1..4),
                };
                let offset = rng.random_range(0..8) * BLOCK;
                let written = data(rng, blocks * BLOCK);
                if let Err(err) =
                    cell.write(ExtentId::new(CELL.id, sequence), offset, &written).await
                {
                    return Some(*err.downcast_ref::<DiskFull>().expect("refused for room alone"));
                }
                let end = self.open.entry(sequence).or_insert(0);
                *end = (*end).max(offset + blocks * BLOCK);
            }
            6 | 7 => self.commit(cell).await,
            8 if !self.sealed.is_empty() => {
                let sequence = self.sealed.swap_remove(rng.random_range(0..self.sealed.len()));
                cell.free(ExtentId::new(CELL.id, sequence)).unwrap();
            }
            _ => cell.close_segment().unwrap(),
        }
        None
    }

    async fn commit(&mut self, cell: &LocalCell<MemDir>) {
        cell.commit(None).await.unwrap();
        self.sealed.extend(std::mem::take(&mut self.open).into_keys());
    }
}

/// A cell in `dir` of the tests' options.
fn new_cell(dir: &MemDir) -> LocalCell<MemDir> {
    let description = Description { block_size: BLOCK as u32, capacity: 1 << 30, cell: CELL };
    LocalCell::create(dir.clone(), description, OPTIONS).unwrap()
}

/// Runs a cell's own work to its end, as if its guest wrote nothing more: what it took at its
/// peak past what it took before.
async fn drain(cell: &LocalCell<MemDir>, dir: &MemDir) -> u64 {
    let footprint = dir.footprint();
    let before = footprint.bytes();
    footprint.take_peak();
    cell.commit(None).await.unwrap();
    cell.close_segment().unwrap();
    cell.commit(None).await.unwrap();
    cell.wait_framed().await.unwrap();
    cell.flush().await.unwrap();
    footprint.take_peak() - before
}

/// A cell's own work grows it within its reserve, measured at random points of random lives:
/// sparse extents, which a commit copies whole, holes and all, compressible data framed in copies,
/// large extents in raw files of their own. The worst found, with its reserve's constant, was
/// 1.08 times its unframed and open bytes (`host_disk::RESERVE_FACTOR`); and its footprint counts
/// its open extents as its guest wrote them.
#[tokio::test(flavor = "multi_thread")]
async fn a_cells_own_work_grows_it_within_its_reserve() {
    let mut worst: f64 = 0.0;
    for seed in 0..100 {
        let dir = MemDir::new();
        let cell = new_cell(&dir);
        let mut guest = Guest::new(seed);
        for at in 0..200 {
            assert_eq!(guest.step(&cell).await, None);
            let footprint = dir.footprint();
            assert_eq!(footprint.open(), guest.open.values().sum::<u64>(), "seed {seed}, {at}");
            if guest.rng.random_range(0..8) == 0 {
                let reserved = footprint.reserved();
                let grew = drain(&cell, &dir).await;
                assert!(grew <= reserved, "seed {seed}, {at}: grew {grew}, reserved {reserved}");
                worst = worst.max(grew as f64 / reserved as f64);
                guest.sealed.extend(std::mem::take(&mut guest.open).into_keys());
            }
        }
    }
    eprintln!("the most a cell's own work took of its reserve: {:.0}%", worst * 100.0);
}

/// What each commit's records add to its open segment's log, as measured: 115 bytes or so, about
/// 120 KiB for 1,000 commits, then its framed footer's. No admission covers them: commits are never
/// refused, and a guest committing again and again on a full disk takes this much each time, which
/// the disk's headroom absorbs (TASK-539.15).
const RECORDS: u64 = 256 << 10;

/// A disk as full as admission lets its cells take it, its allowance its capacity less a headroom
/// for commits' records ([`RECORDS`]), refuses its guests' writes, with `DiskFull`, before its own
/// `ENOSPC`: every commit, closing, framing and flush finds room, and the cell goes on, over many
/// lives.
#[tokio::test(flavor = "multi_thread")]
async fn a_full_disk_refuses_guests_and_never_its_cells_own_work() {
    for seed in 0..40 {
        let capacity = 2 << 20;
        let dir = MemDir::with_capacity(capacity);
        dir.disk().disk().govern(capacity - RECORDS, RECORDS, false);
        let cell = new_cell(&dir);
        let mut guest = Guest::new(seed);
        let mut refused = 0;
        for _ in 0..600 {
            if guest.step(&cell).await.is_some() {
                refused += 1;
                guest.commit(&cell).await;
                cell.flush().await.unwrap();
            }
        }
        drain(&cell, &dir).await;
        assert!(refused > 0, "seed {seed}: never full");
        assert!(dir.footprint().bytes() <= capacity, "seed {seed}");
    }
}
