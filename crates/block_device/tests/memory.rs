// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What a block device's flushes hold in memory, counted by an allocator that counts: a flush
//! copies its data once (TASK-583), and discards cost what holds data alone (TASK-584).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use block_device::{BlockDevice, Options};
use storage_device::DeviceHolder;
use storage_device::fake_device::FakeDevice;

/// The heap's bytes now, and the most since [`measure`] last began.
static HELD: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// SAFETY: the system's allocator's, counted.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as the caller promises.
        let memory = unsafe { System.alloc(layout) };
        if !memory.is_null() {
            let held = HELD.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(held, Ordering::Relaxed);
        }
        memory
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as the caller promises.
        let memory = unsafe { System.alloc_zeroed(layout) };
        if !memory.is_null() {
            let held = HELD.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(held, Ordering::Relaxed);
        }
        memory
    }

    unsafe fn dealloc(&self, memory: *mut u8, layout: Layout) {
        HELD.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: as the caller promises.
        unsafe { System.dealloc(memory, layout) }
    }

    unsafe fn realloc(&self, memory: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: as the caller promises.
        let moved = unsafe { System.realloc(memory, layout, size) };
        if !moved.is_null() {
            HELD.fetch_sub(layout.size(), Ordering::Relaxed);
            let held = HELD.fetch_add(size, Ordering::Relaxed) + size;
            PEAK.fetch_max(held, Ordering::Relaxed);
        }
        moved
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

const PAGE: usize = 4096;
const MIB: usize = 1 << 20;

/// What `f` held at most beyond what was held when it began, and what it left held beyond that.
fn measure(f: impl FnOnce()) -> (usize, isize) {
    let before = HELD.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    f();
    let (peak, after) = (PEAK.load(Ordering::Relaxed), HELD.load(Ordering::Relaxed));
    (peak - before, after as isize - before as isize)
}

fn blocks() -> std::sync::Arc<BlockDevice> {
    let device = FakeDevice::new(1 << 19, PAGE as u32).calm();
    BlockDevice::open(DeviceHolder::new(device), Options::default()).unwrap()
}

/// TASK-583: a flush of 64 MiB written holds, besides the data written and what the device keeps
/// of it, the data once more at most: the plan's copy. It held three copies: the written chunks
/// cloned, the plan's, and each extent's copy to write.
#[test]
fn a_flush_copies_its_data_once() {
    let blocks = blocks();
    let dirty = 64 * MIB;
    let data: Vec<u8> = (0..dirty).map(|i| (i / PAGE) as u8 ^ i as u8).collect();
    blocks.write(0, &data).unwrap();
    drop(data);
    let (peak, left) = measure(|| blocks.flush().unwrap());
    // The device keeps what it was written, and the written chunks are gone: what is left beyond
    // the written chunks' bytes is the device's.
    let device = (left + dirty as isize) as usize;
    let beyond = peak - device;
    assert!(beyond < dirty + dirty / 10, "a flush of {dirty} held {beyond} beyond the device's");
    let mut read = vec![0; PAGE];
    blocks.read(5 * PAGE as u64, &mut read).unwrap();
    assert_eq!(read[1], 5 ^ 1);
}

/// TASK-584: discarding 1 GiB of holes, whole, holds nothing and leaves nothing to flush;
/// discarding 64 MiB of data notes it as one range, not a page at a time, and reads as zeros once
/// flushed.
#[test]
fn discarding_holds_what_holds_data_alone() {
    let blocks = blocks();
    let size = 1u64 << 30;
    blocks.resize(size).unwrap();
    blocks.flush().unwrap();
    let (peak, _) = measure(|| {
        blocks.discard(0, size).unwrap();
        blocks.flush().unwrap();
    });
    assert!(peak < 64 << 10, "discarding holes held {peak} bytes");

    let data = 64 * MIB;
    blocks.write(0, &vec![7; data]).unwrap();
    blocks.flush().unwrap();
    let (peak, _) = measure(|| blocks.discard(0, size).unwrap());
    // The nodes naming its 16384 pages, read already, and the range: not 16384 entries.
    assert!(peak < 64 << 10, "discarding 64 MiB of data held {peak} bytes");
    blocks.flush().unwrap();
    let mut read = vec![1; PAGE];
    blocks.read(4096 * PAGE as u64, &mut read).unwrap();
    assert_eq!(read, vec![0; PAGE]);
}
