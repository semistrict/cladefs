// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use extent_files::{FileSet, Options};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use storage_device::DeviceHolder;
use storage_device::fake_device::{FakeDevice, Op, RoundTrips};

const PAGE: usize = 4096;

fn files_with_read_fault() -> (Arc<FileSet>, Arc<AtomicBool>) {
    let fail = Arc::new(AtomicBool::default());
    let trigger = Arc::clone(&fail);
    let mut device = FakeDevice::new(1 << 16, PAGE as u32);
    device.set_op_callback(move |op| {
        if matches!(op, Op::Read) && trigger.swap(false, Ordering::SeqCst) {
            anyhow::bail!("injected device read error");
        }
        Ok(())
    });
    (FileSet::open(DeviceHolder::new(device), Options::default()).unwrap(), fail)
}

fn reopen(files: &FileSet) -> Arc<FileSet> {
    FileSet::open(files.device().snapshot().unwrap(), Options::default()).unwrap()
}

fn contents(files: &FileSet, name: &str) -> Vec<u8> {
    let mut bytes = vec![0; files.size(name).unwrap() as usize];
    let read = files.read(name, 0, &mut bytes).unwrap();
    assert_eq!(read, bytes.len());
    bytes
}

/// TASK-516: even an unrelated file's subsequent commit must preserve a file whose deletion failed.
#[test]
fn failed_delete_preserves_the_file_across_an_unrelated_commit() {
    let (files, fail) = files_with_read_fault();
    let data = vec![7; 300 * PAGE];
    files.open_file("victim", true).unwrap();
    files.write("victim", 0, &data).unwrap();
    files.sync("victim").unwrap();
    files.close_file("victim");

    fail.store(true, Ordering::SeqCst);
    let error = files.delete("victim").unwrap_err();
    assert_eq!(error.root_cause().to_string(), "injected device read error");
    let live = files.exists("victim").unwrap();
    files.open_file("other", true).unwrap();
    files.write("other", 0, b"another commit").unwrap();
    files.sync("other").unwrap();
    let recovered = reopen(&files);
    assert_eq!((live, recovered.exists("victim").unwrap()), (true, true));
    recovered.open_file("victim", false).unwrap();
    assert_eq!(contents(&recovered, "victim"), data);
    assert_eq!(recovered.device().extents(), recovered.extents().unwrap());
}

/// TASK-531: on a failed shrink, dirty data beyond the proposed end still belongs to the file.
#[test]
fn failed_partial_page_truncate_preserves_earlier_writes() {
    let (files, fail) = files_with_read_fault();
    files.open_file("image", true).unwrap();
    let mut expected = vec![1; 3 * PAGE];
    files.write("image", 0, &expected).unwrap();
    files.sync("image").unwrap();
    expected[2 * PAGE..].fill(7);
    files.write("image", (2 * PAGE) as u64, &expected[2 * PAGE..]).unwrap();

    fail.store(true, Ordering::SeqCst);
    let error = files.truncate("image", 1).unwrap_err();
    assert_eq!(error.root_cause().to_string(), "injected device read error");
    let live = contents(&files, "image");
    files.sync("image").unwrap();
    let recovered = reopen(&files);
    recovered.open_file("image", false).unwrap();
    assert_eq!((live == expected, contents(&recovered, "image") == expected), (true, true));
}

/// TASK-533: one contiguous run with its metadata already known fits in one device read.
#[test]
fn reading_one_contiguous_node_uses_one_device_request() {
    let trips = Arc::new(RoundTrips::default());
    let device = FakeDevice::new(1 << 16, PAGE as u32).with_round_trips(trips.clone());
    let files = FileSet::open(DeviceHolder::new(device), Options::default()).unwrap();
    let len = files.node_bytes();
    let data: Vec<u8> = (0..len).map(|i| (i / PAGE as u64) as u8).collect();
    files.open_file("image", true).unwrap();
    files.write("image", 0, &data).unwrap();
    files.sync("image").unwrap();
    let runs = files.runs("image", 0, len).unwrap();
    assert_eq!(runs.len(), 1);
    let before = trips.reads.count();
    assert_eq!(contents(&files, "image"), data);
    assert_eq!(trips.reads.count() - before, 1, "device reads for {len} contiguous bytes");
}
