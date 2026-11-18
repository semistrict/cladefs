// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A file used as a VM's memory is (TASK-456.2): batches of pages written and holes punched, each
//! committed at once, crashed at every barrier.

use extent_files::{FileSet, Options};
use rand::{RngExt as _, SeedableRng as _};
use storage_device::DeviceHolder;
use storage_device::fake_device::{Crashes, FakeDevice};

const PAGE: usize = 4096;
const PAGES: usize = 300;

/// The bytes of a memory of `PAGES` pages, page `page` of `round`.
fn page(round: u64, page: usize) -> Vec<u8> {
    let mut bytes = vec![(round as u8).wrapping_mul(31) ^ page as u8; PAGE];
    bytes[..8].copy_from_slice(&(page as u64).to_le_bytes());
    bytes
}

fn read_image(file_set: &FileSet) -> Vec<u8> {
    let mut image = vec![0xee; PAGES * PAGE];
    assert_eq!(file_set.read("memory", 0, &mut image).unwrap(), PAGES * PAGE);
    image
}

/// A crash at any barrier of a run of commits, each of pages written and holes punched, leaves
/// the memory as one commit made it, whole, and the device holding what it names alone.
#[test]
fn a_crash_leaves_the_memory_as_a_commit_made_it() {
    for seed in 0..4 {
        let crashes = Crashes::new(seed, 1);
        let device = FakeDevice::new(1 << 18, PAGE as u32).record_crashes(crashes.clone());
        let options = Options { max_extent_blocks: 64, ..Options::default() };
        let file_set = FileSet::open(DeviceHolder::new(device), options).unwrap();
        file_set.open_file("memory", true).unwrap();
        file_set.truncate("memory", (PAGES * PAGE) as u64).unwrap();
        file_set.sync("memory").unwrap();
        let mut model = vec![0; PAGES * PAGE];
        let mut committed = vec![model.clone()];
        crashes.mark(0);
        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
        for round in 1..=30u64 {
            for _ in 0..rng.random_range(1..40) {
                let at = rng.random_range(0..PAGES);
                let bytes = page(round, at);
                file_set.write("memory", (at * PAGE) as u64, &bytes).unwrap();
                model[at * PAGE..][..PAGE].copy_from_slice(&bytes);
            }
            let start = rng.random_range(0..PAGES);
            let end = (start + rng.random_range(1..30)).min(PAGES);
            file_set.punch("memory", (start * PAGE) as u64, ((end - start) * PAGE) as u64).unwrap();
            model[start * PAGE..end * PAGE].fill(0);
            file_set.sync("memory").unwrap();
            assert_eq!(read_image(&file_set), model, "seed {seed}, round {round}");
            committed.push(model.clone());
            crashes.mark(round);
        }
        file_set.close_file("memory");

        let mut recovered = 0;
        for (mark, device) in crashes.take() {
            let reopened = FileSet::open(DeviceHolder::new(device), options).unwrap();
            assert_eq!(reopened.device().extents(), reopened.extents().unwrap(), "seed {seed}");
            reopened.open_file("memory", false).unwrap();
            let image = read_image(&reopened);
            assert!(image == committed[mark as usize], "seed {seed}, mark {mark}: another image");
            recovered += 1;
        }
        assert_eq!(recovered, 30, "seed {seed}");
    }
}
