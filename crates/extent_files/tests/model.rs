// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A file set under random operations, checked against a model of what each file holds and what
//! its last commit made it: writes, rewrites, holes, truncations, commits, deletes, files opened
//! and closed, pages fetched and given back, and the file set closed and opened again or crashed
//! and opened again, in any order, read from the device or mapped into a window.
//!
//! Every file set a SQLite or memory cell keeps is closed and opened again in its life, after any
//! commit at all; until this test no test closed one after a commit that listed its dying extents
//! in an extent of their own, and closing freed that list, which the next opening must read
//! (TASK-540).

use std::sync::Arc;

use extent_files::testing::CopyingWindow;
use extent_files::{FileSet, Options};
use rand::rngs::StdRng;
use rand::{RngExt as _, SeedableRng as _};
use storage_device::DeviceHolder;
use storage_device::fake_device::FakeDevice;

const PAGE: usize = 4096;
/// The largest a file grows: with `max_extent_blocks` 2, a rewrite of it whole leaves 80 extents
/// dying in one commit, more than a root lists itself.
const MAX_PAGES: usize = 160;
/// More files than a window has regions, so that some read from the device.
const FILES: usize = 3;
const OPTIONS: Options = Options { max_extent_blocks: 2, window_regions: 2 };

/// How the file set reads: from its device, or from a window it maps its files into.
#[derive(Clone, Copy, Debug)]
enum Mode {
    Device,
    Window,
}

#[derive(Clone, Debug)]
enum Op {
    Open {
        file: usize,
    },
    /// Committed first if it has changes: closing a file loses what it hasn't committed.
    Close {
        file: usize,
    },
    Write {
        file: usize,
        at: usize,
        len: usize,
        seed: u8,
    },
    /// Every page of the file written anew, as VACUUM or a whole memory image does.
    Rewrite {
        file: usize,
        seed: u8,
    },
    Punch {
        file: usize,
        page: usize,
        pages: usize,
    },
    Truncate {
        file: usize,
        size: usize,
    },
    Sync {
        file: usize,
    },
    Delete {
        file: usize,
    },
    /// A page as its last commit left it, the `pick`th of them, fetched to read in place, as
    /// SQLite's xFetch does, held until given back.
    Fetch {
        file: usize,
        pick: usize,
    },
    /// The page of the file fetched last given back.
    Unfetch {
        file: usize,
    },
    /// The file set closed and opened again.
    Reopen,
    /// The file set's device crashed, losing nothing, and the file set opened again.
    Crash,
}

#[derive(Clone, Debug, Default)]
struct Model {
    /// What the file holds, open or not.
    live: Vec<u8>,
    /// What its last commit made it, if one did.
    committed: Option<Vec<u8>>,
    open: bool,
}

/// A page fetched and not yet given back, which reads as it did when fetched until it is.
struct Held {
    file: usize,
    page: usize,
    at: *const u8,
    bytes: Vec<u8>,
}

fn name(file: usize) -> String {
    format!("file{file}")
}

fn op(rng: &mut StdRng) -> Op {
    let file = rng.random_range(0..FILES);
    match rng.random_range(0..100) {
        0..9 => Op::Open { file },
        9..14 => Op::Close { file },
        14..38 => Op::Write {
            file,
            at: rng.random_range(0..MAX_PAGES * PAGE),
            len: rng.random_range(1..3 * PAGE),
            seed: rng.random(),
        },
        38..43 => Op::Rewrite { file, seed: rng.random() },
        43..48 => Op::Punch {
            file,
            page: rng.random_range(0..MAX_PAGES),
            pages: rng.random_range(1..MAX_PAGES / 2),
        },
        48..53 => Op::Truncate { file, size: rng.random_range(0..MAX_PAGES * PAGE) },
        53..70 => Op::Sync { file },
        70..75 => Op::Delete { file },
        75..85 => Op::Fetch { file, pick: rng.random_range(0..MAX_PAGES) },
        85..90 => Op::Unfetch { file },
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
    file_set: Arc<FileSet>,
    models: Vec<Model>,
    held: Vec<Held>,
    history: Vec<String>,
}

impl Run {
    fn new(seed: u64, mode: Mode) -> Self {
        let device = DeviceHolder::new(FakeDevice::new(1 << 18, PAGE as u32));
        Self {
            seed,
            mode,
            file_set: Self::open(device, mode).unwrap(),
            models: vec![Model::default(); FILES],
            held: Vec::new(),
            history: Vec::new(),
        }
    }

    fn open(device: DeviceHolder, mode: Mode) -> anyhow::Result<Arc<FileSet>> {
        match mode {
            Mode::Device => FileSet::open(device, OPTIONS),
            Mode::Window => {
                let window = Box::new(CopyingWindow::new(PAGE as u32));
                FileSet::open_mapped(device, window, OPTIONS)
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

    fn contents(&self, file: usize) -> Vec<u8> {
        let size = self.file_set.size(&name(file)).unwrap() as usize;
        let mut read = vec![0xee; size];
        let got = self.file_set.read(&name(file), 0, &mut read).unwrap();
        assert_eq!(got, size, "{}: a short read of {}", self.context(), name(file));
        read
    }

    fn check_open(&self, file: usize) {
        let model = &self.models[file];
        let read = self.contents(file);
        assert!(read == model.live, "{}: {} differs from its model", self.context(), name(file));
    }

    /// Every page held reads as it did when fetched.
    fn check_held(&self) {
        for held in &self.held {
            // SAFETY: a page fetched and not given back, which the window keeps mapped.
            let now = unsafe { std::slice::from_raw_parts(held.at, PAGE) };
            assert!(
                now == held.bytes,
                "{}: page {} of {}, held, changed",
                self.context(),
                held.page,
                name(held.file)
            );
        }
    }

    /// Gives back every page of `file` held, or of every file.
    fn give_back(&mut self, file: Option<usize>) {
        let file_set = &self.file_set;
        self.held.retain(|held| {
            let give = file.is_none_or(|file| file == held.file);
            if give {
                file_set.unfetch(&name(held.file));
            }
            !give
        });
    }

    /// The file set as the models' commits made it, and the device holding what it names alone.
    fn check_committed(&mut self) {
        let mut names: Vec<_> =
            (0..FILES).filter(|&file| self.models[file].committed.is_some()).map(name).collect();
        names.sort();
        let context = self.context();
        assert_eq!(self.file_set.files().unwrap(), names, "{context}");
        assert_eq!(self.file_set.device().extents(), self.file_set.extents().unwrap(), "{context}");
        for file in 0..FILES {
            if self.models[file].committed.is_some() {
                self.file_set.open_file(&name(file), false).unwrap();
                self.check_open(file);
                self.file_set.close_file(&name(file));
            }
        }
    }

    /// The file set opened again from its device: what no commit made is gone.
    fn opened_again(&mut self, close: bool) {
        self.give_back(None);
        if close {
            let closed = self.file_set.close();
            closed.unwrap_or_else(|error| panic!("{}: closing: {error:#}", self.context()));
        }
        let device = self.file_set.device().snapshot().unwrap();
        self.file_set = Self::open(device, self.mode)
            .unwrap_or_else(|error| panic!("{}: opening again: {error:#}", self.context()));
        for model in &mut self.models {
            model.open = false;
            match &model.committed {
                Some(committed) => model.live = committed.clone(),
                None => model.live.clear(),
            }
        }
        self.check_committed();
    }

    fn apply(&mut self, op: Op) {
        let file_set = self.file_set.clone();
        let exists = |model: &Model| model.open || model.committed.is_some();
        match op {
            Op::Open { file } => {
                let model = &mut self.models[file];
                if model.open {
                    return;
                }
                file_set.open_file(&name(file), !exists(model)).unwrap();
                model.open = true;
                if model.committed.is_none() {
                    model.live.clear();
                }
            }
            Op::Close { file } => {
                if !self.models[file].open {
                    return;
                }
                self.give_back(Some(file));
                let model = &mut self.models[file];
                if model.committed.is_some() && model.committed.as_ref() != Some(&model.live) {
                    file_set.sync(&name(file)).unwrap();
                    model.committed = Some(model.live.clone());
                }
                file_set.close_file(&name(file));
                model.open = false;
            }
            Op::Write { file, at, len, seed } => {
                if !self.models[file].open {
                    return;
                }
                let data = bytes(seed, at, len);
                file_set.write(&name(file), at as u64, &data).unwrap();
                write_model(&mut self.models[file].live, at, &data);
            }
            Op::Rewrite { file, seed } => {
                if !self.models[file].open {
                    return;
                }
                let data = bytes(seed, 0, MAX_PAGES * PAGE);
                file_set.write(&name(file), 0, &data).unwrap();
                write_model(&mut self.models[file].live, 0, &data);
            }
            Op::Punch { file, page, pages } => {
                let model = &mut self.models[file];
                let end = (page + pages).min(model.live.len() / PAGE);
                if !model.open || page >= end {
                    return;
                }
                file_set
                    .punch(&name(file), (page * PAGE) as u64, ((end - page) * PAGE) as u64)
                    .unwrap();
                model.live[page * PAGE..end * PAGE].fill(0);
            }
            Op::Truncate { file, size } => {
                if !self.models[file].open {
                    return;
                }
                file_set.truncate(&name(file), size as u64).unwrap();
                self.models[file].live.resize(size, 0);
            }
            Op::Sync { file } => {
                if !self.models[file].open {
                    return;
                }
                file_set.sync(&name(file)).unwrap();
                self.models[file].committed = Some(self.models[file].live.clone());
                self.check_open(file);
            }
            Op::Delete { file } => {
                let model = &mut self.models[file];
                if model.open {
                    return;
                }
                let was = file_set.delete(&name(file)).unwrap();
                assert_eq!(was, model.committed.is_some(), "{}", self.context());
                *model = Model::default();
            }
            Op::Fetch { file, pick } => {
                let model = &self.models[file];
                let Some(committed) = model.committed.as_ref().filter(|_| model.open) else {
                    return;
                };
                let clean: Vec<_> = (0..model.live.len().min(committed.len()) / PAGE)
                    .filter(|page| {
                        let range = page * PAGE..(page + 1) * PAGE;
                        model.live[range.clone()] == committed[range]
                    })
                    .collect();
                if clean.is_empty() {
                    return;
                }
                let page = clean[pick % clean.len()];
                let fetched = file_set.fetch(&name(file), (page * PAGE) as u64, PAGE as u64);
                let Some(at) = fetched.unwrap() else { return };
                // SAFETY: fetched, so mapped until given back.
                let bytes = unsafe { std::slice::from_raw_parts(at, PAGE) }.to_vec();
                let expected = &model.live[page * PAGE..][..PAGE];
                assert!(
                    bytes == expected,
                    "{}: page {page} of {}, fetched, reads otherwise",
                    self.context(),
                    name(file)
                );
                self.held.push(Held { file, page, at, bytes });
            }
            Op::Unfetch { file } => {
                if let Some(index) = self.held.iter().rposition(|held| held.file == file) {
                    self.held.remove(index);
                    file_set.unfetch(&name(file));
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

/// Random runs of operations keep every file as the model says, open or opened again, every page
/// fetched as it was until given back, and the device holding just what the file set names.
#[test]
fn a_file_set_keeps_what_its_commits_made_through_every_operation() {
    both(|mode| {
        for seed in 0..48 {
            let mut rng = StdRng::seed_from_u64(seed);
            let mut run = Run::new(seed, mode);
            run.run((0..300).map(|_| op(&mut rng)));
            run.opened_again(true);
        }
    });
}

/// The case TASK-540 is: a commit leaving more extents dying than a root lists, then the file set
/// closed and opened again, and closed and opened again with no commit between.
#[test]
fn a_file_set_opens_again_after_a_commit_listing_its_dying_extents() {
    both(|mode| {
        Run::new(0, mode).run([
            Op::Open { file: 0 },
            Op::Rewrite { file: 0, seed: 1 },
            Op::Sync { file: 0 },
            Op::Rewrite { file: 0, seed: 2 },
            Op::Sync { file: 0 },
            Op::Reopen,
            Op::Reopen,
            Op::Open { file: 0 },
            Op::Truncate { file: 0, size: 0 },
            Op::Sync { file: 0 },
            Op::Crash,
            Op::Reopen,
        ])
    });
}

/// The same with the file deleted: a delete of a whole file is a commit freeing its every extent.
#[test]
fn a_file_set_opens_again_after_deleting_a_large_file() {
    both(|mode| {
        Run::new(0, mode).run([
            Op::Open { file: 0 },
            Op::Rewrite { file: 0, seed: 1 },
            Op::Sync { file: 0 },
            Op::Close { file: 0 },
            Op::Delete { file: 0 },
            Op::Reopen,
            Op::Reopen,
        ])
    });
}

/// A page fetched reads as it was through commits that change it, until given back, as SQLite's
/// own VFS remaps nothing while pages are out; given back, the next fetch reads the commit.
#[test]
fn a_fetched_page_reads_as_it_was_until_given_back() {
    let mut run = Run::new(0, Mode::Window);
    run.run([
        Op::Open { file: 0 },
        Op::Write { file: 0, at: 0, len: 4 * PAGE, seed: 1 },
        Op::Sync { file: 0 },
        Op::Fetch { file: 0, pick: 0 },
        Op::Write { file: 0, at: 0, len: 2 * PAGE, seed: 2 },
        Op::Sync { file: 0 },
        Op::Fetch { file: 0, pick: 2 },
    ]);
    assert_eq!(run.held.len(), 2, "{}: both pages fetched", run.context());
    run.run([Op::Unfetch { file: 0 }, Op::Unfetch { file: 0 }, Op::Fetch { file: 0, pick: 0 }]);
    assert_eq!(run.held.len(), 1, "{}: page 0 fetched again", run.context());
}
