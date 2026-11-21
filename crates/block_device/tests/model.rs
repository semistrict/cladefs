// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A block device under random operations, checked against a model of what it holds and what its
//! last flush made it: writes, rewrites, discards, resizes, flushes, pages fetched and given back,
//! and the block device closed and opened again or crashed and opened again, in any order, read
//! from the extent device or mapped into a window.
//!
//! Every block device a SQLite or memory cell keeps is closed and opened again in its life, after
//! any flush at all; until this test no test closed one after a flush that listed its dying
//! extents in an extent of their own, and closing freed that list, which the next opening must
//! read (TASK-540).

use std::sync::Arc;

use block_device::testing::CopyingWindow;
use block_device::{BlockDevice, Options};
use rand::rngs::StdRng;
use rand::{RngExt as _, SeedableRng as _};
use storage_device::DeviceHolder;
use storage_device::fake_device::FakeDevice;

const PAGE: usize = 4096;
/// The largest the contents grow: with `max_extent_blocks` 2, a rewrite of them whole leaves 80
/// extents dying in one flush, more than a root lists itself.
const MAX_PAGES: usize = 160;
const OPTIONS: Options = Options { max_extent_blocks: 2 };
/// Three quarters of the largest contents, so that what is past it reads from the extent device:
/// a window is made, and filled, at each opening.
const WINDOW: u64 = (MAX_PAGES * 3 / 4 * PAGE) as u64;

/// How the block device reads: from its extent device, or from a window it maps its contents into.
#[derive(Clone, Copy, Debug)]
enum Mode {
    Device,
    Window,
}

#[derive(Clone, Debug)]
enum Op {
    Write {
        at: usize,
        len: usize,
        seed: u8,
    },
    /// Every page written anew, as VACUUM or a whole memory image does.
    Rewrite {
        seed: u8,
    },
    Discard {
        page: usize,
        pages: usize,
    },
    Resize {
        size: usize,
    },
    Flush,
    /// A page as the last flush left it, the `pick`th of them, fetched to read in place, as
    /// SQLite's xFetch does, held until given back.
    Fetch {
        pick: usize,
    },
    /// The page fetched last given back.
    Unfetch,
    /// The block device closed and opened again.
    Reopen,
    /// The extent device crashed, losing nothing, and the block device opened again.
    Crash,
}

/// A page fetched and not yet given back, which reads as it did when fetched until it is.
struct Held {
    page: usize,
    at: *const u8,
    bytes: Vec<u8>,
}

fn op(rng: &mut StdRng) -> Op {
    match rng.random_range(0..100) {
        0..30 => Op::Write {
            at: rng.random_range(0..MAX_PAGES * PAGE),
            len: rng.random_range(1..3 * PAGE),
            seed: rng.random(),
        },
        30..36 => Op::Rewrite { seed: rng.random() },
        36..43 => Op::Discard {
            page: rng.random_range(0..MAX_PAGES),
            pages: rng.random_range(1..MAX_PAGES / 2),
        },
        43..50 => Op::Resize { size: rng.random_range(0..MAX_PAGES * PAGE) },
        50..70 => Op::Flush,
        70..82 => Op::Fetch { pick: rng.random_range(0..MAX_PAGES) },
        82..90 => Op::Unfetch,
        90..95 => Op::Reopen,
        _ => Op::Crash,
    }
}

fn bytes(seed: u8, at: usize, len: usize) -> Vec<u8> {
    (at..at + len).map(|i| seed ^ (i as u8) ^ (i / PAGE) as u8).collect()
}

fn write_model(live: &mut Vec<u8>, at: usize, data: &[u8]) {
    if live.len() < at + data.len() {
        live.resize(at + data.len(), 0);
    }
    live[at..][..data.len()].copy_from_slice(data);
}

struct Run {
    seed: u64,
    mode: Mode,
    blocks: Arc<BlockDevice>,
    /// What the contents hold.
    live: Vec<u8>,
    /// What the last flush made them.
    flushed: Vec<u8>,
    held: Vec<Held>,
    history: Vec<String>,
}

impl Run {
    fn new(seed: u64, mode: Mode) -> Self {
        let device = DeviceHolder::new(FakeDevice::new(1 << 18, PAGE as u32));
        Self {
            seed,
            mode,
            blocks: Self::open(device, mode).unwrap(),
            live: Vec::new(),
            flushed: Vec::new(),
            held: Vec::new(),
            history: Vec::new(),
        }
    }

    fn open(device: DeviceHolder, mode: Mode) -> anyhow::Result<Arc<BlockDevice>> {
        match mode {
            Mode::Device => BlockDevice::open(device, OPTIONS),
            Mode::Window => {
                let window = Box::new(CopyingWindow::new(WINDOW, PAGE as u32));
                BlockDevice::open_mapped(device, window, OPTIONS)
            }
        }
    }

    fn run(&mut self, ops: impl IntoIterator<Item = Op>) {
        for op in ops {
            self.history.push(format!("{op:?}"));
            self.apply(op);
            self.check_held();
        }
    }

    fn context(&self) -> String {
        let from = self.history.len().saturating_sub(40);
        let ops = self.history[from..].join("\n  ");
        format!("seed {}, {:?}, last ops:\n  {ops}", self.seed, self.mode)
    }

    fn check_contents(&self) {
        let size = self.blocks.size().unwrap() as usize;
        let mut read = vec![0xee; size];
        let got = self.blocks.read(0, &mut read).unwrap();
        assert_eq!(got, size, "{}: a short read", self.context());
        assert!(read == self.live, "{}: the contents differ from the model", self.context());
    }

    /// Every page held reads as it did when fetched.
    fn check_held(&self) {
        for held in &self.held {
            // SAFETY: a page fetched and not given back, which the window keeps mapped.
            let now = unsafe { std::slice::from_raw_parts(held.at, PAGE) };
            assert!(now == held.bytes, "{}: page {}, held, changed", self.context(), held.page);
        }
    }

    /// Gives back every page held.
    fn give_back(&mut self) {
        for _ in self.held.drain(..) {
            self.blocks.unfetch();
        }
    }

    /// The block device opened again from its extent device: what no flush made is gone, and the
    /// extent device holds what it names alone.
    fn opened_again(&mut self, close: bool) {
        self.give_back();
        if close {
            let closed = self.blocks.close();
            closed.unwrap_or_else(|error| panic!("{}: closing: {error:#}", self.context()));
        }
        let device = self.blocks.device().snapshot().unwrap();
        self.blocks = Self::open(device, self.mode)
            .unwrap_or_else(|error| panic!("{}: opening again: {error:#}", self.context()));
        self.live = self.flushed.clone();
        let context = self.context();
        assert_eq!(self.blocks.device().extents(), self.blocks.extents().unwrap(), "{context}");
        self.check_contents();
    }

    fn apply(&mut self, op: Op) {
        let blocks = self.blocks.clone();
        match op {
            Op::Write { at, len, seed } => {
                let data = bytes(seed, at, len);
                blocks.write(at as u64, &data).unwrap();
                write_model(&mut self.live, at, &data);
            }
            Op::Rewrite { seed } => {
                let data = bytes(seed, 0, MAX_PAGES * PAGE);
                blocks.write(0, &data).unwrap();
                write_model(&mut self.live, 0, &data);
            }
            Op::Discard { page, pages } => {
                let end = (page + pages).min(self.live.len() / PAGE);
                if page >= end {
                    return;
                }
                blocks.discard((page * PAGE) as u64, ((end - page) * PAGE) as u64).unwrap();
                self.live[page * PAGE..end * PAGE].fill(0);
            }
            Op::Resize { size } => {
                blocks.resize(size as u64).unwrap();
                self.live.resize(size, 0);
            }
            Op::Flush => {
                blocks.flush().unwrap();
                self.flushed = self.live.clone();
                self.check_contents();
            }
            Op::Fetch { pick } => {
                let clean: Vec<_> = (0..self.live.len().min(self.flushed.len()) / PAGE)
                    .filter(|page| {
                        let range = page * PAGE..(page + 1) * PAGE;
                        self.live[range.clone()] == self.flushed[range]
                    })
                    .collect();
                if clean.is_empty() {
                    return;
                }
                let page = clean[pick % clean.len()];
                let fetched = blocks.fetch((page * PAGE) as u64, PAGE as u64);
                let Some(at) = fetched.unwrap() else { return };
                // SAFETY: fetched, so mapped until given back.
                let bytes = unsafe { std::slice::from_raw_parts(at, PAGE) }.to_vec();
                let expected = &self.live[page * PAGE..][..PAGE];
                assert!(
                    bytes == expected,
                    "{}: page {page}, fetched, reads otherwise",
                    self.context()
                );
                self.held.push(Held { page, at, bytes });
            }
            Op::Unfetch => {
                if self.held.pop().is_some() {
                    blocks.unfetch();
                }
            }
            Op::Reopen => self.opened_again(true),
            Op::Crash => self.opened_again(false),
        }
    }
}

/// Runs `test` in each mode.
fn both(test: impl Fn(Mode)) {
    test(Mode::Device);
    test(Mode::Window);
}

/// Random runs of operations keep the contents as the model says, as they are or opened again,
/// every page fetched as it was until given back, and the extent device holding just what the
/// block device names.
#[test]
fn a_block_device_keeps_what_its_flushes_made_through_every_operation() {
    both(|mode| {
        for seed in 0..48 {
            let mut rng = StdRng::seed_from_u64(seed);
            let mut run = Run::new(seed, mode);
            run.run((0..300).map(|_| op(&mut rng)));
            run.opened_again(true);
        }
    });
}

/// The case TASK-540 is: a flush leaving more extents dying than a root lists, then the block
/// device closed and opened again, and closed and opened again with no flush between.
#[test]
fn a_block_device_opens_again_after_a_flush_listing_its_dying_extents() {
    both(|mode| {
        Run::new(0, mode).run([
            Op::Rewrite { seed: 1 },
            Op::Flush,
            Op::Rewrite { seed: 2 },
            Op::Flush,
            Op::Reopen,
            Op::Reopen,
            Op::Resize { size: 0 },
            Op::Flush,
            Op::Crash,
            Op::Reopen,
        ])
    });
}

/// The same with the contents discarded whole, a flush freeing every extent: once a file's
/// deletion.
#[test]
fn a_block_device_opens_again_after_discarding_its_contents() {
    both(|mode| {
        Run::new(0, mode).run([
            Op::Rewrite { seed: 1 },
            Op::Flush,
            Op::Discard { page: 0, pages: MAX_PAGES },
            Op::Flush,
            Op::Reopen,
            Op::Reopen,
        ])
    });
}

/// A page fetched reads as it was through flushes that change it, until given back, as SQLite's
/// own VFS remaps nothing while pages are out; given back, the next fetch reads the flush.
#[test]
fn a_fetched_page_reads_as_it_was_until_given_back() {
    let mut run = Run::new(0, Mode::Window);
    run.run([
        Op::Write { at: 0, len: 4 * PAGE, seed: 1 },
        Op::Flush,
        Op::Fetch { pick: 0 },
        Op::Write { at: 0, len: 2 * PAGE, seed: 2 },
        Op::Flush,
        Op::Fetch { pick: 2 },
    ]);
    assert_eq!(run.held.len(), 2, "{}: both pages fetched", run.context());
    run.run([Op::Unfetch, Op::Unfetch, Op::Fetch { pick: 0 }]);
    assert_eq!(run.held.len(), 1, "{}: page 0 fetched again", run.context());
}

/// TASK-553: a map that fails while a page is held, fetched, leaves the window, which the page
/// reads in place: the page reads as it was, what the failed map was for reads from the extent
/// device, and once the page is given back, reads map again.
#[test]
fn a_failed_map_leaves_the_window_while_pages_are_held() {
    let device = DeviceHolder::new(FakeDevice::new(1 << 18, PAGE as u32));
    let window = CopyingWindow::new(WINDOW, PAGE as u32);
    let failing = window.failing();
    let blocks = BlockDevice::open_mapped(device, Box::new(window), OPTIONS).unwrap();
    let a = bytes(1, 0, 8 * PAGE);
    blocks.write(0, &a).unwrap();
    blocks.flush().unwrap();
    let held = blocks.fetch(0, PAGE as u64).unwrap().expect("the first page mapped");
    // SAFETY: a page fetched and not given back, which the window keeps mapped.
    let read_held = || unsafe { std::slice::from_raw_parts(held, PAGE) }.to_vec();
    assert!(read_held() == a[..PAGE]);

    // A read reaching a page the window maps not yet maps it, which fails: the device's.
    failing.store(true, std::sync::atomic::Ordering::Relaxed);
    let mut page = vec![0; PAGE];
    blocks.read(4 * PAGE as u64, &mut page).unwrap();
    assert!(page == a[4 * PAGE..5 * PAGE]);
    assert!(read_held() == a[..PAGE], "the held page, its map failed");
    failing.store(false, std::sync::atomic::Ordering::Relaxed);

    // Given back, the window maps what reads reach again.
    blocks.unfetch();
    let mut read = vec![0; 8 * PAGE];
    blocks.read(0, &mut read).unwrap();
    assert!(read == a);
    let fetched = blocks.fetch(4 * PAGE as u64, PAGE as u64).unwrap().expect("mapped again");
    // SAFETY: fetched, until given back.
    assert!(unsafe { std::slice::from_raw_parts(fetched, PAGE) } == &a[4 * PAGE..5 * PAGE]);
    blocks.unfetch();
}
