// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use block_device::testing::CopyingWindow;
use block_device::{BlockDevice, Options};
use futures::executor::block_on;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use storage_device::DeviceHolder;
use storage_device::fake_device::{FakeDevice, Op, RoundTrips};

const PAGE: usize = 4096;

fn blocks_with_read_fault() -> (Arc<BlockDevice>, Arc<AtomicBool>) {
    let fail = Arc::new(AtomicBool::default());
    let trigger = Arc::clone(&fail);
    let mut device = FakeDevice::new(1 << 16, PAGE as u32);
    device.set_op_callback(move |op| {
        if matches!(op, Op::Read) && trigger.swap(false, Ordering::SeqCst) {
            anyhow::bail!("injected device read error");
        }
        Ok(())
    });
    (BlockDevice::open(DeviceHolder::new(device), Options::default()).unwrap(), fail)
}

fn reopen(blocks: &BlockDevice) -> Arc<BlockDevice> {
    BlockDevice::open(blocks.device().snapshot().unwrap(), Options::default()).unwrap()
}

fn contents(blocks: &BlockDevice) -> Vec<u8> {
    let mut bytes = vec![0; blocks.size().unwrap() as usize];
    let read = blocks.read(0, &mut bytes).unwrap();
    assert_eq!(read, bytes.len());
    bytes
}

/// TASK-516, once a failed delete of a file: a discard whose read of the tree fails leaves the
/// contents as they were, which a later flush keeps.
#[test]
fn a_failed_discard_leaves_the_contents_across_a_flush() {
    let (blocks, fail) = blocks_with_read_fault();
    let data = vec![7; 300 * PAGE];
    blocks.write(0, &data).unwrap();
    blocks.flush().unwrap();
    // Opened again on the same device, so that the discard reads the tree, which fails.
    let taken = blocks.device().take_when_dropped();
    drop(blocks);
    let blocks = BlockDevice::open(block_on(taken), Options::default()).unwrap();
    fail.store(true, Ordering::SeqCst);
    let error = blocks.discard(0, data.len() as u64).unwrap_err();
    assert_eq!(error.root_cause().to_string(), "injected device read error");
    let live = contents(&blocks);
    blocks.write(data.len() as u64, b"another flush").unwrap();
    blocks.flush().unwrap();
    let recovered = reopen(&blocks);
    assert_eq!((live == data, contents(&recovered)[..data.len()] == data[..]), (true, true));
    assert_eq!(recovered.device().extents(), recovered.extents().unwrap());
}

/// TASK-531: on a failed shrink, what was written past the proposed end still belongs to the
/// contents.
#[test]
fn failed_partial_page_resize_preserves_earlier_writes() {
    let (blocks, fail) = blocks_with_read_fault();
    let mut expected = vec![1; 3 * PAGE];
    blocks.write(0, &expected).unwrap();
    blocks.flush().unwrap();
    expected[2 * PAGE..].fill(7);
    blocks.write((2 * PAGE) as u64, &expected[2 * PAGE..]).unwrap();

    fail.store(true, Ordering::SeqCst);
    let error = blocks.resize(1).unwrap_err();
    assert_eq!(error.root_cause().to_string(), "injected device read error");
    let live = contents(&blocks);
    blocks.flush().unwrap();
    let recovered = reopen(&blocks);
    assert_eq!((live == expected, contents(&recovered) == expected), (true, true));
}

/// TASK-533: one contiguous run with its metadata already known fits in one device read.
#[test]
fn reading_one_contiguous_node_uses_one_device_request() {
    let trips = Arc::new(RoundTrips::default());
    let device = FakeDevice::new(1 << 16, PAGE as u32).with_round_trips(trips.clone());
    let blocks = BlockDevice::open(DeviceHolder::new(device), Options::default()).unwrap();
    let len = blocks.node_bytes();
    let data: Vec<u8> = (0..len).map(|i| (i / PAGE as u64) as u8).collect();
    blocks.write(0, &data).unwrap();
    blocks.flush().unwrap();
    let runs = blocks.runs(0, len).unwrap();
    assert_eq!(runs.len(), 1);
    let before = trips.reads.count();
    assert_eq!(contents(&blocks), data);
    assert_eq!(trips.reads.count() - before, 1, "device reads for {len} contiguous bytes");
}

fn mapped() -> Arc<BlockDevice> {
    let device = DeviceHolder::new(FakeDevice::new(1 << 18, PAGE as u32));
    let window = Box::new(CopyingWindow::new(64 << 20, PAGE as u32));
    BlockDevice::open_mapped(device, window, Options::default()).unwrap()
}

/// TASK-530, once a file's region of the window given back as it closed: a block device opened
/// mapped maps from its opening, a fetch of what it first flushed reading in place.
#[test]
fn a_mapped_device_fetches_its_first_flush_in_place() {
    let blocks = mapped();
    assert_eq!(blocks.fetch(0, PAGE as u64).unwrap(), None, "nothing flushed yet");
    blocks.write(0, &[5; PAGE]).unwrap();
    blocks.flush().unwrap();
    let fetched = blocks.fetch(0, PAGE as u64).unwrap().expect("a page in place");
    // SAFETY: fetched, until given back.
    assert_eq!(unsafe { std::slice::from_raw_parts(fetched, PAGE) }, [5; PAGE]);
    blocks.unfetch();
}

/// The window maps what a flush changed again only once no page is fetched: a page fetched
/// reads as it was meanwhile, and what the flush changed reads as flushed, from the device.
#[test]
fn a_flush_waits_for_fetched_pages_to_remap() {
    let blocks = mapped();
    blocks.write(0, &[1; 2 * PAGE]).unwrap();
    blocks.flush().unwrap();
    let fetched = blocks.fetch(PAGE as u64, PAGE as u64).unwrap().expect("a page in place");
    // SAFETY: fetched, until given back.
    let page = || unsafe { std::slice::from_raw_parts(fetched, PAGE) }.to_vec();
    assert_eq!(page(), vec![1; PAGE]);

    blocks.write(PAGE as u64, &[2; PAGE]).unwrap();
    assert_eq!(blocks.fetch(PAGE as u64, PAGE as u64).unwrap(), None, "changed since the flush");
    blocks.flush().unwrap();
    assert_eq!(page(), vec![1; PAGE], "held as fetched");
    assert_eq!(blocks.fetch(PAGE as u64, PAGE as u64).unwrap(), None, "the window is behind");
    let mut read = vec![0; 2 * PAGE];
    assert_eq!(blocks.read(0, &mut read).unwrap(), 2 * PAGE);
    assert_eq!(read[PAGE..], [2; PAGE]);

    blocks.unfetch();
    let fetched = blocks.fetch(PAGE as u64, PAGE as u64).unwrap().expect("mapped again");
    // SAFETY: fetched, until given back.
    assert_eq!(unsafe { std::slice::from_raw_parts(fetched, PAGE) }, [2; PAGE]);
    blocks.unfetch();
}
