// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: how long a cell's hot paths take a call, each looking up an extent's life: a page-in's
//! `locate`, a write's opening and finish, and a free; the cell holding
//! tens of thousands of extents, keeping as many freed, and having spent their numbers
//! (TASK-160.18). A measurement, not a check: ignored, run in release,
//! `cargo nextest run --release -p local_cell --test hot_path --run-ignored only --no-capture`.

use local_cell::{Description, HostDir, LocalCell, Options};
use std::time::Instant;
use storage_device::ExtentId;

const BLOCK: usize = 4096;
const CELL: bucket_cell::layout::CellId = bucket_cell::layout::test_cell(7);
/// The extents the cell holds, and as many it keeps, freed.
const EXTENTS: u64 = 20_000;
/// The rounds of each call timed, over every extent.
const ROUNDS: u32 = 5;

fn extent(sequence: u64) -> ExtentId {
    ExtentId::new(CELL.id, sequence)
}

/// The nanoseconds a call of `call` took, on average, over `calls` calls.
async fn timed(calls: u64, call: impl AsyncFnOnce()) -> f64 {
    let started = Instant::now();
    call().await;
    started.elapsed().as_nanos() as f64 / calls as f64
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "a benchmark: --run-ignored only, in release"]
async fn hot_paths() {
    let temp = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let description = Description { block_size: BLOCK as u32, capacity: 4 << 30, cell: CELL };
    let dir = HostDir::new(temp.path()).unwrap();
    let cell = LocalCell::create(dir, description, Options::default()).unwrap();
    let data = vec![7u8; BLOCK];
    // Held: 1..=EXTENTS; kept, freed: EXTENTS+1..=2*EXTENTS, their numbers spent.
    for sequence in 1..=2 * EXTENTS {
        cell.write(extent(sequence), 0, &data).await.unwrap();
        if sequence % 1000 == 0 {
            cell.commit(None).await.unwrap();
        }
    }
    for sequence in EXTENTS + 1..=2 * EXTENTS {
        cell.free(extent(sequence)).unwrap();
    }
    cell.commit(None).await.unwrap();
    let held = || (1..=EXTENTS).map(extent);
    let mut results = Vec::new();
    for _ in 0..ROUNDS {
        let locate = timed(EXTENTS, async || {
            for extent in held() {
                cell.locate(extent, 0, BLOCK as u64).unwrap();
            }
        })
        .await;
        // Refused: sealed, or spent, as a write to a sealed extent is.
        let refused = timed(2 * EXTENTS, async || {
            for sequence in 1..=2 * EXTENTS {
                assert!(cell.begin_write(extent(sequence), 0, BLOCK as u64).await.is_err());
            }
        })
        .await;
        results.push((locate, refused));
    }
    // Writes of new extents, opened and finished, and frees of them, open: once each.
    let first = 2 * EXTENTS + 1;
    let written = timed(EXTENTS, async || {
        for sequence in first..first + EXTENTS {
            cell.write(extent(sequence), 0, &data).await.unwrap();
        }
    })
    .await;
    let freed = timed(EXTENTS, async || {
        for sequence in first..first + EXTENTS {
            cell.free(extent(sequence)).unwrap();
        }
    })
    .await;
    let best =
        |pick: fn(&(f64, f64)) -> f64| results.iter().map(pick).fold(f64::INFINITY, f64::min);
    eprintln!(
        "ns a call, best of {ROUNDS}: locate {:.0}, begin_write refused {:.0}; \
         once: write (open, write, finish) {written:.0}, free (open) {freed:.0}",
        best(|r| r.0),
        best(|r| r.1),
    );
}
