// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the cell's writer, the closer of its open segment and its uploader, run together under
//! Shuttle (awslabs/shuttle), whose scheduler chooses every interleaving of their threads at each
//! lock, condition variable and file operation, and replays a failing one from its seed
//! (TASK-44.4).
//!
//! The writer writes extents (some in segments, some large enough for files of their own), frees
//! some, and commits, ordered or durably, recording what the cell holds as of each commit. The
//! closer closes the open segment between commits, as cladefs's uploader has the cell do. The
//! uploader, as cladefs's does, closes it too and takes each segment closed once its commits are
//! durable; it reads what each of the segment's commits wrote and the extents its footer names
//! (from the cell, or kept for it if freed since), and releases its commits. Their history is
//! checked:
//!
//! - every extent a commit wrote reads, for the uploader, as the writer wrote it, though freed
//!   since, until the uploader releases the commit;
//! - every extent a closed segment's footer names, the segment holds as the writer wrote it as of
//!   the segment's last commit, or its file of its own does, and it reads so for the uploader;
//! - once all is done, the cell holds what the writer's last commit left;
//! - the cell, crashed and reopened, holds at least every commit the uploader took: it only
//!   takes the segments of durable ones.
//!
//! Commits are made durable by a flusher, too, while an uploader begins, as of the latest commit,
//! and while the cell closes: the uploader takes only the segments of commits a flush made durable,
//! and the cell, closed, holds its last commit durably. A free races a write in flight, too, which
//! is dropped, whenever it finishes; and writes fill segments between commits, which go to the next
//! as each is full. And the cell fails, and closes, while a commit waits for a write in progress,
//! which fails then. And the guest's requests are served while a commit's flush writes its
//! checkpoint, stalling (TASK-92.56).
//!
//! So are the crash tests' rounds, the cell framing segments and syncing flushes on threads of its
//! own while their workloads go on, crashing as those interleave, on schedules seeded by the rounds
//! (crash.rs).
//!
//! Each rule the cell relies on for these ([`Rules`]) is turned off by a test that expects a
//! failure: a check that never fails proves nothing. Run with the `shuttle` feature:
//! `cargo nextest run -p local_cell --features shuttle --test shuttle`.

#![cfg(feature = "shuttle")]

use base32_id::Padded;
use extent_frame::segment::{Chain, Footer, NewVersion};
use local_cell::{
    BucketData, Description, Dir, LocalCell, MemDir, Opening, Options, Rules, set_rules,
};
use shuttle::future::block_on;
use shuttle::scheduler::{PctScheduler, RandomScheduler, ReplayScheduler};
use shuttle::sync::Mutex;
use shuttle::sync::atomic::{AtomicBool, Ordering};
use shuttle::thread;
use shuttle::{Config, Runner};
use std::collections::BTreeMap;
use std::sync::Arc;
use storage_device::ExtentId;

mod crash;
mod stalling;

use stalling::{Op, Stalling};

const BLOCK: usize = 4096;
const CELL: bucket_cell::layout::CellId = bucket_cell::layout::test_cell(7);
/// Small segments, so that commits and the closer close several, and an extent of four blocks in
/// a file of its own.
const OPTIONS: Options = Options {
    max_extent: 64 << 20,
    segment_size: 3 * BLOCK as u64,
    segment_age: std::time::Duration::from_secs(3600),
    large_extent: 4 * BLOCK as u64,
    open_files: 2,
    // cladefs: flushes of either kind, holding their runs or checking them (TASK-301).
    held_most: 8 << 10,
};
/// The chain of a writer of the cell in object storage, whose segments the cell closes.
const WRITER: Chain = Chain::new(CELL, 0, 0, 0);

/// The schedules each test runs, randomly and by PCT: some seconds; `SHUTTLE_SOAK` multiplies them.
fn runs() -> usize {
    let soak: usize = std::env::var("SHUTTLE_SOAK").map_or(1, |soak| soak.parse().unwrap());
    1500 * soak
}

fn extent(sequence: u64) -> ExtentId {
    ExtentId::new(CELL.id, sequence)
}

/// Bytes each their offset modulo 251, plus `seed`.
fn pattern(seed: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8 ^ seed).collect()
}

/// What the cell holds as of a commit: each extent's data.
type Holds = BTreeMap<ExtentId, Vec<u8>>;

/// The writer's steps: extents written (sequence, blocks), freed, commits, and a version's, which
/// commits durably.
#[derive(Clone, Copy)]
enum Step {
    Write(u64, usize),
    Free(u64),
    Commit { durable: bool },
    Version,
}

const STEPS: &[Step] = &[
    Step::Write(1, 1),
    Step::Write(2, 4),
    Step::Commit { durable: false },
    Step::Write(3, 2),
    Step::Free(1),
    Step::Commit { durable: true },
    Step::Free(2),
    Step::Write(4, 1),
    Step::Version,
    Step::Commit { durable: false },
    Step::Free(3),
    Step::Write(5, 1),
    Step::Commit { durable: true },
];

/// One run: the writer, the closer and the uploader, then the checks. cladefs: the uploader
/// acknowledging what it takes, the bucket keeps nothing of the frees past their segments'
/// acknowledgements (TASK-165.9).
fn run() {
    let steps = STEPS;
    let dir = MemDir::new();
    let description = Description { block_size: BLOCK as u32, capacity: 1 << 20, cell: CELL };
    let cell = Arc::new(LocalCell::create(dir.clone(), description, OPTIONS).unwrap());
    // What the cell holds as of each commit, by epoch, the writer's record.
    let history = Arc::new(Mutex::new(vec![Holds::new()]));
    let written = Arc::new(AtomicBool::new(false));
    // The version's commit, which the segment naming the version ends at.
    let forked = Arc::new(Mutex::new(None::<u64>));

    let writer = {
        let (cell, history, written, forked) =
            (cell.clone(), history.clone(), written.clone(), forked.clone());
        thread::spawn(move || {
            block_on(async move {
                let mut holds = Holds::new();
                for step in steps {
                    match *step {
                        Step::Write(sequence, blocks) => {
                            let data = pattern(sequence as u8, blocks * BLOCK);
                            cell.write(extent(sequence), 0, &data).await.unwrap();
                            holds.insert(extent(sequence), data);
                        }
                        Step::Free(sequence) => {
                            cell.free(extent(sequence)).unwrap();
                            holds.remove(&extent(sequence));
                        }
                        Step::Commit { .. } | Step::Version => {
                            match *step {
                                Step::Version => {
                                    let cut = NewVersion { id: 0xa, keep_last: None };
                                    *forked.lock().unwrap() =
                                        Some(cell.commit_version(cut, false).await.unwrap());
                                }
                                Step::Commit { durable: true } => cell.commit(None).await.unwrap(),
                                _ => cell.commit_ordered(None).await.unwrap(),
                            }
                            let mut history = history.lock().unwrap();
                            assert_eq!(cell.last_commit().unwrap(), history.len() as u64);
                            history.push(holds.clone());
                        }
                    }
                }
                written.store(true, Ordering::SeqCst);
            })
        })
    };

    let closer = {
        let cell = cell.clone();
        thread::spawn(move || {
            block_on(async move {
                for _ in 0..3 {
                    cell.close_now().await.unwrap();
                    thread::yield_now();
                }
            })
        })
    };

    // The last commit the uploader took, which the cell must recover at least; and what it
    // uploaded, which the cell reads in the bucket once acknowledged there.
    let taken = Arc::new(Mutex::new(0));
    let bucket = Bucket::default();
    let uploader = {
        let (cell, dir, history, written, taken, forked, bucket) = (
            cell.clone(),
            dir.clone(),
            history.clone(),
            written.clone(),
            taken.clone(),
            forked.clone(),
            bucket.clone(),
        );
        thread::spawn(move || {
            block_on(async move {
                // What the cell held as of commit `epoch`, which the writer records once the commit
                // returns: its record may come later.
                let held = |epoch: u64| loop {
                    if let Some(holds) = history.lock().unwrap().get(epoch as usize) {
                        break holds.clone();
                    }
                    thread::yield_now();
                };
                loop {
                    // Whether the writer was done before this round: then it takes everything.
                    let done = written.load(Ordering::SeqCst);
                    // As cladefs's uploader does (upload.rs): closes the open segment between
                    // commits, and takes the segments closed whose commits are durable, reading
                    // them and their extents' own files.
                    cell.close_now().await.unwrap();
                    // Taking them opens their extents' own files, which the cell keeps for it
                    // though their extents are freed.
                    let segments = cell
                        .closed(usize::MAX)
                        .await
                        .unwrap_or_else(|error| panic!("taking the closed segments: {error:#}"));
                    for closed in segments {
                        let last = closed.last();
                        // Each commit it holds, after those of the segments taken before: what the
                        // commit wrote reads, for the uploader, as it was written, though freed
                        // since; and the host crashing now, the cell recovers at least the commit.
                        let after = *taken.lock().unwrap();
                        for epoch in after + 1..=last {
                            for seed in [epoch, epoch + 100] {
                                let crashed = bucket.reopen(dir.crash(seed));
                                let recovered = crashed.last_commit().unwrap();
                                assert!(
                                    recovered >= epoch,
                                    "uploaded commit {epoch} before durable: a crash recovered \
                                 {recovered}, seed {seed}"
                                );
                            }
                            *taken.lock().unwrap() = epoch;
                        }
                        let holds = held(last);
                        let data = extent_frame::data(
                            &closed.bytes().unwrap(),
                            &bucket_cell::testing::ring([CELL]).of(CELL.id),
                        )
                        .unwrap();
                        let own: BTreeMap<_, _> = closed.own_bytes().unwrap().into_iter().collect();
                        bucket.put(bucket_key(&closed), data.to_vec());
                        bucket.acknowledge(closed.footer.clone(), closed.data_len);
                        for (key, bytes) in &own {
                            bucket.put(
                                key.clone(),
                                extent_frame::data(
                                    bytes,
                                    &bucket_cell::testing::ring([CELL]).of(CELL.id),
                                )
                                .unwrap()
                                .to_vec(),
                            );
                        }
                        let in_segment =
                            closed.footer.contents.extents.iter().map(|&(extent, offset, len)| {
                                (
                                    extent,
                                    len,
                                    data[offset as usize..(offset + len) as usize].to_vec(),
                                )
                            });
                        let on_their_own = closed.footer.own().map(|(extent, location)| {
                            let data = extent_frame::data(
                                &own[&location.key],
                                &bucket_cell::testing::ring([CELL]).of(CELL.id),
                            )
                            .unwrap();
                            (extent, location.len, data)
                        });
                        // Each extent its footer names, as its last commit left it: in it, or in a
                        // file of its own of the write-ahead log, though freed since (board 562).
                        for (extent, len, uploaded) in in_segment.chain(on_their_own) {
                            assert!(
                                Some(&uploaded) == holds.get(&extent)
                                    && uploaded.len() as u64 == len,
                                "{extent:?} of commit {last} uploads other data than was written"
                            );
                        }
                        // A version's ends at the version, as its commit closed it (doc-18).
                        if !closed.footer.contents.versions.is_empty() {
                            let fork = loop {
                                if let Some(epoch) = *forked.lock().unwrap() {
                                    break epoch;
                                }
                                thread::yield_now();
                            };
                            assert!(
                                last == fork,
                                "a version's segment ends after it: at commit {last}, of {fork}"
                            );
                        }
                        // Uploaded: acknowledged, the frees of its commits, whose extents' own
                        // files it may name, are the bucket's, and it may go.
                        cell.acknowledged(closed.number).await.unwrap();
                    }
                    if done {
                        return;
                    }
                    thread::yield_now();
                }
            })
        })
    };

    writer.join().unwrap();
    closer.join().unwrap();
    uploader.join().unwrap();

    // The cell holds what the writer's last commit left, and nothing it freed.
    let holds = history.lock().unwrap().last().unwrap().clone();
    for (extent, data) in &holds {
        let mut read = vec![0; data.len()];
        bucket.read(&mut read, |read| cell.read(*extent, 0, read)).unwrap();
        assert!(&read == data, "{extent:?} reads other data than was written");
    }
    let held: Vec<_> = cell.extents().unwrap().into_iter().map(|(extent, _)| extent).collect();
    assert_eq!(held, holds.keys().copied().collect::<Vec<_>>());

    // Crashed, at any of these versions, the cell recovers at least what the uploader took.
    let taken = *taken.lock().unwrap();
    for seed in 0..4 {
        let crashed = bucket.reopen(dir.crash(seed));
        let epoch = crashed.last_commit().unwrap();
        assert!(epoch >= taken, "crash {seed}: recovered commit {epoch}, uploaded {taken}");
    }

    // Its open segment closed, its footer names where the extents its commits touched are.
    cell.close_segment().unwrap();
    block_on(cell.commit(None)).expect("the last commit failed");
}

/// cladefs: what the uploader uploaded, as far as the cell's reads need it once their data is in
/// the bucket: each object's data, unframed, by its key (TASK-165.8); and the footers the uploader
/// acknowledged, in order, the cell's history there, which a crash's cell opens as of
/// (TASK-165.9).
#[derive(Clone, Default)]
struct Bucket(Arc<Mutex<BTreeMap<String, Vec<u8>>>>, Arc<Mutex<Vec<(Footer, u64)>>>);

impl Bucket {
    fn put(&self, key: String, data: Vec<u8>) {
        self.0.lock().unwrap().insert(key, data);
    }

    /// The segment whose footer is `footer`, its data `len` bytes long, is acknowledged: the
    /// cell's from now on.
    fn acknowledge(&self, footer: Footer, len: u64) {
        self.1.lock().unwrap().push((footer, len));
    }

    /// The cell in `dir`, opened as the bucket holds its cell: its acknowledged footers
    /// replayed, its writer's chain after them.
    fn reopen(&self, dir: MemDir) -> LocalCell<MemDir> {
        let (mut state, mut chain) = (bucket_cell::State::new(CELL), WRITER);
        for (footer, len) in self.1.lock().unwrap().iter() {
            state.apply(footer, *len);
            chain = chain.admit(footer).expect("the footers acknowledged follow each other");
        }
        let head =
            bucket_cell::layout::Head::default().holding(chain.epoch(), chain.next_segment());
        let taken = bucket_cell::TakenCell::of(head, &state, chain);
        let keys = bucket_cell::testing::ring([CELL]);
        let opening =
            Opening { state, layer: None, taken: Some(taken), cache: None, keys, held: None };
        LocalCell::recover(dir).unwrap().open(OPTIONS, opening).unwrap()
    }

    /// `read`, a read of an extent from its start into `buf`; or, its data being in the bucket,
    /// what its error says is there, as cladefs's fetcher reads it.
    fn read(
        &self,
        buf: &mut [u8],
        read: impl FnOnce(&mut [u8]) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let error = match read(buf) {
            Err(error) if error.downcast_ref::<BucketData>().is_some() => error,
            read => return read,
        };
        let location = &error.downcast_ref::<BucketData>().unwrap().location;
        let objects = self.0.lock().unwrap();
        let Some(object) = objects.get(&location.key) else {
            anyhow::bail!("{error:#}, which the uploader uploaded not");
        };
        let data = &object[location.offset as usize..][..location.len as usize];
        let len = buf.len().min(data.len());
        buf[..len].copy_from_slice(&data[..len]);
        buf[len..].fill(0);
        Ok(())
    }
}

/// cladefs: the key in the bucket of the object `closed`, a segment closed, is: its writer's.
fn bucket_key<F>(closed: &local_cell::Closed<F>) -> String {
    bucket_cell::layout::segment(closed.footer.cell, closed.footer.segment, closed.footer.epoch)
}

/// One run of writes racing commits, as a VM's requests may: a writer writes extents, in segments
/// and in a file of its own, while a committer commits and the closer closes the open segment
/// between commits. Each extent a commit sealed holds what was written to it, and the segments
/// closed after hold what their commits sealed, in them.
fn racing() {
    block_on(async {
        let description = Description { block_size: BLOCK as u32, capacity: 1 << 20, cell: CELL };
        let cell = Arc::new(LocalCell::create(MemDir::new(), description, OPTIONS).unwrap());
        // Extents of one block, in segments, and of four, in a file of its own.
        const WRITES: [(u64, usize); 3] = [(1, 1), (2, 4), (3, 1)];
        let writer = {
            let cell = cell.clone();
            thread::spawn(move || {
                block_on(async move {
                    for (sequence, blocks) in WRITES {
                        cell.write(extent(sequence), 0, &pattern(sequence as u8, blocks * BLOCK))
                            .await
                            .unwrap();
                    }
                    // A write begun in a segment and abandoned, as a failed one is: commits and
                    // closings waiting for it go on.
                    drop(cell.begin_write(extent(9), 0, BLOCK as u64).await.unwrap());
                })
            })
        };
        let committer = {
            let cell = cell.clone();
            thread::spawn(move || {
                block_on(async move {
                    for _ in 0..2 {
                        cell.commit_ordered(None).await.expect("a write raced a commit");
                    }
                })
            })
        };
        let closer = {
            let cell = cell.clone();
            thread::spawn(move || {
                block_on(async move {
                    for _ in 0..2 {
                        cell.close_now().await.expect("a write raced a commit");
                        thread::yield_now();
                    }
                })
            })
        };
        for thread in [writer, committer, closer] {
            thread.join().unwrap();
        }
        cell.commit(None).await.expect("a write raced a commit");
        cell.close_segment().unwrap();
        cell.commit(None).await.expect("a write raced a commit");
        // The framings the closings began end first, as no commit waits for them (board 562): a
        // failure found below unwinds with no framer holding the cell's locks, which Shuttle, a
        // task panicking, can't wait for, and aborts.
        cell.wait_framed().await.expect("a write raced a commit");
        for (sequence, blocks) in WRITES {
            let mut data = vec![0; blocks * BLOCK];
            cell.read(extent(sequence), 0, &mut data).expect("a write raced a commit");
            assert!(
                data == pattern(sequence as u8, blocks * BLOCK),
                "a write raced a commit: extent {sequence} reads other data"
            );
        }
    })
}

/// Segments of three blocks, and extents of four at most: a segment holds seven blocks at most,
/// past which writes between commits go to another (TASK-79.2).
const ROLLING: Options = Options { max_extent: 4 * BLOCK as u64, ..OPTIONS };

/// One run of writes filling segments between commits, as a VM's requests may (TASK-79.2): a
/// writer writes extents of two blocks, in segments, while a committer commits and the closer
/// closes the open segment between commits. The open segment, full, waits for the next commit, or
/// the closer's, holding commits' data, and writes go to the next, while commits seal what was
/// written, where it is (TASK-92.41). Each extent reads as written, and the cell, crashed, opens
/// again holding what its last commit did.
fn rolling_over() {
    block_on(async {
        let dir = MemDir::new();
        let description = Description { block_size: BLOCK as u32, capacity: 1 << 20, cell: CELL };
        let cell = Arc::new(LocalCell::create(dir.clone(), description, ROLLING).unwrap());
        const EXTENTS: u64 = 6;
        let writer = {
            let cell = cell.clone();
            thread::spawn(move || {
                block_on(async move {
                    for sequence in 1..=EXTENTS {
                        let data = pattern(sequence as u8, 2 * BLOCK);
                        cell.write(extent(sequence), 0, &data)
                            .await
                            .expect("a write filling segments failed");
                    }
                })
            })
        };
        let committer = {
            let cell = cell.clone();
            thread::spawn(move || {
                block_on(async move {
                    for _ in 0..2 {
                        cell.commit_ordered(None)
                            .await
                            .expect("a commit of writes filling segments failed");
                    }
                })
            })
        };
        let closer = {
            let cell = cell.clone();
            thread::spawn(move || {
                block_on(async move {
                    for _ in 0..2 {
                        cell.close_now().await.expect("a closing of segments being filled failed");
                        thread::yield_now();
                    }
                })
            })
        };
        for thread in [writer, committer, closer] {
            thread.join().unwrap();
        }
        cell.commit(None).await.expect("a commit of writes filling segments failed");
        let check = |cell: &LocalCell<MemDir>| {
            for sequence in 1..=EXTENTS {
                let mut data = vec![0; 2 * BLOCK];
                cell.read(extent(sequence), 0, &mut data).unwrap();
                assert!(
                    data == pattern(sequence as u8, 2 * BLOCK),
                    "extent {sequence}, written filling segments, reads other data"
                );
            }
        };
        check(&cell);
        check(&LocalCell::open(dir.crash(0), ROLLING).unwrap());
    })
}

/// One run of a free racing a write in flight, as a VM's requests may (TASK-79.1): a writer
/// begins a write of an extent of four blocks, in a file of its own, and finishes it; once it has
/// begun, a freer frees the extent, and writes its second block again, in a segment. The write the
/// free raced is dropped, whenever it finishes: the extent holds the freer's write alone, and the
/// cell commits, and opens again holding that.
fn freeing_during_a_write() {
    block_on(async {
        let dir = MemDir::new();
        let description = Description { block_size: BLOCK as u32, capacity: 1 << 20, cell: CELL };
        let cell = Arc::new(LocalCell::create(dir.clone(), description, OPTIONS).unwrap());
        let begun = Arc::new(AtomicBool::new(false));
        let writer = {
            let (cell, begun) = (cell.clone(), begun.clone());
            thread::spawn(move || {
                block_on(async move {
                    let data = pattern(1, 4 * BLOCK);
                    let write = cell.begin_write(extent(1), 0, data.len() as u64).await.unwrap();
                    begun.store(true, Ordering::SeqCst);
                    write.write(&data, 0).unwrap();
                    write.finish();
                })
            })
        };
        let freer = {
            let cell = cell.clone();
            thread::spawn(move || {
                block_on(async move {
                    while !begun.load(Ordering::SeqCst) {
                        thread::yield_now();
                    }
                    cell.free(extent(1)).unwrap();
                    cell.write(extent(1), BLOCK as u64, &pattern(2, BLOCK))
                        .await
                        .expect("a write to a freed extent landed: the extent is sealed");
                })
            })
        };
        for thread in [writer, freer] {
            thread.join().unwrap();
        }
        let written = [vec![0; BLOCK], pattern(2, BLOCK)].concat();
        let check = |cell: &LocalCell<MemDir>| {
            let held = cell.extents().unwrap();
            assert!(
                held == [(extent(1), 2 * BLOCK as u64)],
                "a write to a freed extent landed: the cell holds {held:?}"
            );
            let mut read = vec![0; 2 * BLOCK];
            cell.read(extent(1), 0, &mut read).unwrap();
            assert!(
                read == written,
                "a write to a freed extent landed: the extent reads other data"
            );
        };
        check(&cell);
        cell.commit(None).await.expect("a write to a freed extent landed: the commit failed");
        drop(Arc::into_inner(cell).unwrap());
        check(
            &LocalCell::open(dir, OPTIONS)
                .expect("a write to a freed extent landed: no cell opens"),
        );
    })
}

/// One run of an uploader beginning while commits are made durable: a writer orders commits, a
/// flusher makes them durable, and the uploader begins, as cladefs's does, as of the cell's latest
/// commit, closes the open segment between commits, and takes the segments closed. A crash
/// recovers at least the last commit of each segment it took: it takes only those of commits a
/// flush made durable, not those of commits a flush took and has yet to sync.
fn uploading_during_flushes() {
    let dir = MemDir::new();
    let description = Description { block_size: BLOCK as u32, capacity: 1 << 20, cell: CELL };
    let cell = Arc::new(LocalCell::create(dir.clone(), description, OPTIONS).unwrap());
    let writer = {
        let cell = cell.clone();
        thread::spawn(move || {
            block_on(async move {
                for sequence in 1..=3 {
                    cell.write(extent(sequence), 0, &pattern(sequence as u8, BLOCK)).await.unwrap();
                    cell.commit_ordered(None).await.unwrap();
                }
            })
        })
    };
    let flusher = {
        let cell = cell.clone();
        thread::spawn(move || {
            block_on(async move {
                for _ in 0..3 {
                    cell.flush().await.unwrap();
                    thread::yield_now();
                }
            })
        })
    };
    let uploader = {
        let cell = cell.clone();
        thread::spawn(move || {
            block_on(async move {
                cell.close_now().await.unwrap();
                for closed in cell.closed(usize::MAX).await.unwrap() {
                    let crashed = LocalCell::open(dir.crash(closed.last()), OPTIONS).unwrap();
                    let epoch = crashed.last_commit().unwrap();
                    assert!(
                        epoch >= closed.last(),
                        "uploaded a segment of commit {} before durable: a crash recovered {epoch}",
                        closed.last()
                    );
                }
            })
        })
    };
    for thread in [writer, flusher, uploader] {
        thread.join().unwrap();
    }
}

/// One run of the cell failing while a commit waits for a write in progress (TASK-83): a writer
/// begins a write, which it finishes once the cell failed; a committer commits, which waits for
/// it; and a flush fails the cell meanwhile. The commit fails then, as all after a failure do:
/// the cell's state may differ from its files.
fn failing_during_a_commit() {
    block_on(async {
        let dir = MemDir::new();
        let description = Description { block_size: BLOCK as u32, capacity: 1 << 20, cell: CELL };
        let cell = Arc::new(LocalCell::create(dir.clone(), description, OPTIONS).unwrap());
        cell.write(extent(1), 0, &pattern(1, BLOCK)).await.unwrap();
        cell.commit_ordered(None).await.unwrap();
        let (begun, failed) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let writer = {
            let (cell, begun, failed) = (cell.clone(), begun.clone(), failed.clone());
            thread::spawn(move || {
                block_on(async move {
                    let write = cell.begin_write(extent(2), 0, BLOCK as u64).await.unwrap();
                    write.write(&pattern(2, BLOCK), 0).unwrap();
                    begun.store(true, Ordering::SeqCst);
                    while !failed.load(Ordering::SeqCst) {
                        thread::yield_now();
                    }
                    write.finish();
                })
            })
        };
        let committer = {
            let (cell, begun) = (cell.clone(), begun.clone());
            thread::spawn(move || {
                block_on(async move {
                    while !begun.load(Ordering::SeqCst) {
                        thread::yield_now();
                    }
                    cell.commit_ordered(None).await
                })
            })
        };
        let flusher = {
            let cell = cell.clone();
            thread::spawn(move || {
                block_on(async move {
                    // Once the commit waits for the write.
                    while !cell.holding_writes() {
                        thread::yield_now();
                    }
                    dir.fail_after(Some(0));
                    let flushed = cell.flush().await;
                    dir.fail_after(None);
                    failed.store(true, Ordering::SeqCst);
                    flushed
                })
            })
        };
        writer.join().unwrap();
        assert!(flusher.join().unwrap().is_err(), "a flush failing succeeded");
        let committed = committer.join().unwrap().map_err(|error| error.to_string());
        // Saying why: the flush's failure (board 562), at its first step, appending the records,
        // which go before the data in a directory that does one thing at a time (board 1700).
        let failed = "the extent store failed: committing the extent store: appending commits' \
                      records: the file system crashed";
        assert!(
            committed == Err(failed.to_owned()),
            "a commit went on over a cell that failed: {committed:?}"
        );
    })
}

/// One run of the cell closing while a commit waits for a write in progress (TASK-83): a writer
/// begins a write, which it finishes once the cell closed; a committer commits, which waits for
/// it; and the cell closes meanwhile, after which it may be opened again. The commit fails
/// then, changing nothing, as all changes of a closed cell do.
fn closing_during_a_commit() {
    block_on(async {
        let dir = MemDir::new();
        let description = Description { block_size: BLOCK as u32, capacity: 1 << 20, cell: CELL };
        let cell = Arc::new(LocalCell::create(dir.clone(), description, OPTIONS).unwrap());
        let (begun, closed) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let writer = {
            let (cell, begun, closed) = (cell.clone(), begun.clone(), closed.clone());
            thread::spawn(move || {
                block_on(async move {
                    let write = cell.begin_write(extent(1), 0, BLOCK as u64).await.unwrap();
                    write.write(&pattern(1, BLOCK), 0).unwrap();
                    begun.store(true, Ordering::SeqCst);
                    while !closed.load(Ordering::SeqCst) {
                        thread::yield_now();
                    }
                    write.finish();
                })
            })
        };
        let committer = {
            let (cell, begun) = (cell.clone(), begun.clone());
            thread::spawn(move || {
                block_on(async move {
                    while !begun.load(Ordering::SeqCst) {
                        thread::yield_now();
                    }
                    cell.commit_ordered(None).await
                })
            })
        };
        let closer = {
            let cell = cell.clone();
            thread::spawn(move || {
                block_on(async move {
                    while !cell.holding_writes() {
                        thread::yield_now();
                    }
                    cell.close().await.unwrap();
                    closed.store(true, Ordering::SeqCst);
                })
            })
        };
        writer.join().unwrap();
        closer.join().unwrap();
        let committed = committer.join().unwrap().map_err(|error| error.to_string());
        assert!(
            committed == Err(format!("cell {CELL} is closed")),
            "a commit went on over a cell closed: {committed:?}"
        );
    })
}

/// One run of the cell closing while a flush makes its commits durable: once closed, it holds
/// its last commit durably, as a later opening of it, or a crash, finds.
fn closing_during_a_flush() {
    block_on(async {
        let dir = MemDir::new();
        let description = Description { block_size: BLOCK as u32, capacity: 1 << 20, cell: CELL };
        let cell = Arc::new(LocalCell::create(dir.clone(), description, OPTIONS).unwrap());
        for sequence in 1..=2 {
            cell.write(extent(sequence), 0, &pattern(sequence as u8, BLOCK)).await.unwrap();
            cell.commit_ordered(None).await.unwrap();
        }
        let flusher = {
            let cell = cell.clone();
            thread::spawn(move || block_on(async move { cell.flush().await.unwrap() }))
        };
        let closer = {
            let cell = cell.clone();
            thread::spawn(move || {
                block_on(async move {
                    cell.close().await.unwrap();
                    let epoch =
                        LocalCell::open(dir.crash(0), OPTIONS).unwrap().last_commit().unwrap();
                    assert!(
                        epoch == 2,
                        "a crash after the cell closed recovered commit {epoch} of 2"
                    );
                })
            })
        };
        for thread in [flusher, closer] {
            thread.join().unwrap();
        }
    })
}

/// A cell whose open segment, its first, holds a commit's extent, in a directory
/// that stalls the creation of the segment's framed file, as the segment closes: so that a closing
/// between commits stalls framing it, as on a slow disk (TASK-41.1).
async fn framing_stalls() -> (Stalling, Arc<LocalCell<Stalling>>) {
    let dir = Stalling::new();
    let description = Description { block_size: BLOCK as u32, capacity: 1 << 20, cell: CELL };
    let cell = Arc::new(LocalCell::create(dir.clone(), description, OPTIONS).unwrap());
    cell.write(extent(1), 0, &pattern(1, BLOCK)).await.unwrap();
    cell.commit(None).await.unwrap();
    dir.arm(Op::Create, &format!("segments/{}/framing-{}", Padded(CELL.id), Padded(0)));
    (dir, cell)
}

/// A closing between commits of a cell [`framing_stalls`] made, on a thread of its own, which
/// must close its segment.
fn spawn_closing(cell: &Arc<LocalCell<Stalling>>) -> thread::JoinHandle<()> {
    let cell = cell.clone();
    thread::spawn(move || {
        block_on(async move {
            let closed = cell.close_now().await;
            let closed = closed.expect("a closing between commits raced the cell's closing");
            assert!(closed, "a closing between commits closed nothing");
        })
    })
}

/// Reads extent 1 of a cell [`framing_stalls`] made, through `cell`, which must be as written.
fn check_framed(cell: &LocalCell<Stalling>, failure: &str) {
    let mut read = vec![0; BLOCK];
    cell.read(extent(1), 0, &mut read).unwrap_or_else(|error| panic!("{failure}: {error:#}"));
    assert!(read == pattern(1, BLOCK), "{failure}: extent 1 reads other data");
}

/// One run of a guest's requests while a closing between commits frames the open segment, which
/// stalls (TASK-41.1): the cell serves its writes, reads and commits meanwhile, holding none of
/// its locks as it frames, as a commit's closing does not. Shuttle, whose waits that can't end are
/// deadlocks it reports, finds so.
fn serving_during_a_framing() {
    block_on(async {
        let (dir, cell) = framing_stalls().await;
        let closing = spawn_closing(&cell);
        dir.wait_until_stalled();
        cell.write(extent(2), 0, &pattern(2, BLOCK)).await.unwrap();
        check_framed(&cell, "a read while a closing between commits frames");
        cell.commit_ordered(None).await.unwrap();
        dir.let_go(Ok(()));
        closing.join().unwrap();
        cell.flush().await.unwrap();
        check_framed(&cell, "a read once a closing between commits framed");
    })
}

/// One run of the cell closing while a closing between commits frames the segment, which stalls
/// (TASK-41.1): the cell's closing waits for it, so that another opening of the cell then
/// finds the segment framed, rather than framing it again meanwhile from its raw file.
fn closing_during_a_framing() {
    block_on(async {
        let (dir, cell) = framing_stalls().await;
        let closing = spawn_closing(&cell);
        dir.wait_until_stalled();
        let reopening = {
            let (cell, dir) = (cell.clone(), dir.clone());
            thread::spawn(move || {
                block_on(async move {
                    cell.close().await.unwrap();
                    let reopened = LocalCell::open(dir, OPTIONS);
                    let reopened =
                        reopened.expect("a closing between commits raced the cell's closing");
                    check_framed(&reopened, "a closing between commits raced the cell's closing");
                })
            })
        };
        dir.let_go(Ok(()));
        closing.join().unwrap();
        reopening.join().unwrap();
    })
}

/// One run of the framing of the segment a commit closed, which stalls, failing, while those of
/// the segments later commits closed wait behind it (TASK-79.99): the cell's closing waits for
/// each, so that another opening of the cell frames their segments again from their raw
/// files only once nothing else frames them. Framings are one queue (TASK-165.9): those after the
/// failed one fail with it, framing nothing. A flush waits for no framing: the commits it made
/// durable while the first stalled, their records in the logs of the segments they were made in,
/// the opening recovers; the commit after, which no flush took before the cell failed, not.
fn closing_after_a_failed_framing() {
    block_on(async {
        let (dir, cell) = framing_stalls().await;
        // Each commit fills the open segment, and closes it: the first, whose framing stalls.
        cell.write(extent(2), 0, &pattern(2, 2 * BLOCK)).await.unwrap();
        cell.commit_ordered(None).await.unwrap();
        cell.write(extent(3), 0, &pattern(3, 3 * BLOCK)).await.unwrap();
        cell.commit_ordered(None).await.unwrap();
        cell.flush().await.unwrap();
        cell.write(extent(4), 0, &pattern(4, 3 * BLOCK)).await.unwrap();
        cell.commit_ordered(None).await.unwrap();
        dir.wait_until_stalled();
        dir.let_go(Err(std::io::Error::other("the disk failed")));
        // The framings end, failing, and the cell with them.
        let error = cell.closed(usize::MAX).await.map(|_| ()).expect_err("a framing failed");
        drop(error);
        cell.close()
            .await
            .expect_err("a cell whose framing failed closed with its commits durable");
        let failure = "a framing raced the cell's closing after a failed framing";
        let reopened = LocalCell::open(dir.clone(), OPTIONS);
        let reopened = reopened.unwrap_or_else(|error| panic!("{failure}: {error:#}"));
        // What was durable: the commits the flush took, extents 1 to 3's.
        assert_eq!(
            reopened.last_commit().unwrap(),
            3,
            "{failure}: other commits than durable recovered"
        );
        check_framed(&reopened, failure);
        // Writing on, it closes segments of its own, numbered past those it found, framed where no
        // framing of the cell before frames them still, making files of their numbers.
        for sequence in 5..8 {
            let written =
                reopened.write(extent(sequence), 0, &pattern(sequence as u8, 3 * BLOCK)).await;
            written.unwrap_or_else(|error| panic!("{failure}: {error:#}"));
            let committed = reopened.commit(None).await;
            committed.unwrap_or_else(|error| panic!("{failure}: {error:#}"));
        }
    })
}

/// A cell, its segments laid out as [`ROLLING`] says, whose open segment, its first,
/// holds a commit's extent, and those of three written since, and is full, in a directory that
/// stalls the creation of the segment's framed file: so that the next commit, which seals what it
/// holds where it is, closes it, and its framing stalls, as on a slow disk (TASK-79.2, TASK-92.41).
async fn framing_at_a_commit_stalls() -> (Stalling, Arc<LocalCell<Stalling>>) {
    let dir = Stalling::new();
    let description = Description { block_size: BLOCK as u32, capacity: 1 << 20, cell: CELL };
    let cell = Arc::new(LocalCell::create(dir.clone(), description, ROLLING).unwrap());
    cell.write(extent(1), 0, &pattern(1, BLOCK)).await.unwrap();
    cell.commit(None).await.unwrap();
    // Six blocks more: the seven it holds at most.
    for sequence in 2..5 {
        cell.write(extent(sequence), 0, &pattern(sequence as u8, 2 * BLOCK)).await.unwrap();
    }
    dir.arm(Op::Create, &format!("segments/{}/framing-{}", Padded(CELL.id), Padded(0)));
    (dir, cell)
}

/// Reads extents 1 to `last` of a cell [`framing_at_a_commit_stalls`] made, and wrote to, through
/// `cell`, which must be as written.
fn check_rolled(cell: &LocalCell<Stalling>, last: u64, failure: &str) {
    for sequence in 1..=last {
        let len = if sequence == 1 { BLOCK } else { 2 * BLOCK };
        let mut read = vec![0; len];
        let reading = cell.read(extent(sequence), 0, &mut read);
        reading.unwrap_or_else(|error| panic!("{failure}: extent {sequence}: {error:#}"));
        assert!(read == pattern(sequence as u8, len), "{failure}: extent {sequence} differs");
    }
}

/// One run of a write finding no room in the open segment, which holds a commit's data, and
/// filling it, and a commit after, which seals in it what is one run there and closes it
/// (TASK-79.2, TASK-92.41): the segment's framing stalls, but the commit waits not for it, nor do
/// the reads after; the flush after makes the commit durable. Shuttle, whose waits that can't end
/// are deadlocks it reports, finds so.
fn serving_during_a_framing_at_a_commit() {
    block_on(async {
        let (dir, cell) = framing_at_a_commit_stalls().await;
        cell.write(extent(5), 0, &pattern(5, 2 * BLOCK)).await.unwrap();
        cell.commit_ordered(None).await.unwrap();
        dir.wait_until_stalled();
        check_rolled(&cell, 5, "a read while a commit's closing of a segment filled frames");
        dir.let_go(Ok(()));
        cell.flush().await.unwrap();
        check_rolled(&cell, 5, "a read once a commit's closing of a segment filled framed");
        drop(cell);
        check_rolled(&LocalCell::open(dir, OPTIONS).unwrap(), 5, "the cell opened again");
    })
}

/// One run of the cell closing while the framing of a segment filled between commits, which the
/// commit after closed, stalls (TASK-79.2, TASK-92.41): the cell's closing waits for it; another
/// opening of the cell finds it framed, and holds what the commit made.
fn closing_during_a_framing_at_a_commit() {
    block_on(async {
        let (dir, cell) = framing_at_a_commit_stalls().await;
        cell.write(extent(5), 0, &pattern(5, 2 * BLOCK)).await.unwrap();
        cell.commit_ordered(None).await.unwrap();
        dir.wait_until_stalled();
        let reopening = {
            let (cell, dir) = (cell.clone(), dir.clone());
            thread::spawn(move || {
                block_on(async move {
                    cell.close().await.unwrap();
                    let reopened = LocalCell::open(dir, OPTIONS).unwrap();
                    check_rolled(
                        &reopened,
                        5,
                        "a commit's closing of a segment filled raced the closing",
                    );
                })
            })
        };
        dir.let_go(Ok(()));
        reopening.join().unwrap();
    })
}

/// One run of a write filling the open segment, which holds a commit's data, racing an uploader
/// taking what the commit made (TASK-150.2): the segment, full, waits for the next commit, but the
/// uploader's closing between commits closes it (TASK-92.41). Whichever goes first, the uploader,
/// asking once more after, with no commit to come, has the commit's segment.
fn uploading_what_a_segment_filled_holds() {
    block_on(async {
        let dir = MemDir::new();
        let description = Description { block_size: BLOCK as u32, capacity: 1 << 20, cell: CELL };
        let cell = Arc::new(LocalCell::create(dir, description, ROLLING).unwrap());
        cell.write(extent(1), 0, &pattern(1, BLOCK)).await.unwrap();
        cell.commit(None).await.unwrap();
        let writer = {
            let cell = cell.clone();
            thread::spawn(move || {
                block_on(async move {
                    for sequence in 2..7 {
                        cell.write(extent(sequence), 0, &pattern(sequence as u8, 2 * BLOCK))
                            .await
                            .unwrap();
                    }
                })
            })
        };
        let mut taken = Vec::new();
        gather(&cell, &mut taken).await;
        writer.join().unwrap();
        gather(&cell, &mut taken).await;
        assert_eq!(taken, [1], "no commit's segment reached the uploader");
    })
}

/// As an uploader gathers for a commit waiting for it, adding what it takes to `taken`: what is
/// closed, closing the rest.
async fn gather(cell: &LocalCell<MemDir>, taken: &mut Vec<u64>) {
    cell.flush().await.unwrap();
    taken.extend(cell.closed(8).await.unwrap().iter().map(|closed| closed.last()));
    cell.close_now().await.unwrap();
    cell.flush().await.unwrap();
    taken.extend(cell.closed(8).await.unwrap().iter().map(|closed| closed.last()));
}

/// One run of writes filling segments, and a commit after, which closes them, sealing in them what
/// is one run in each, racing an uploader taking what the commits made (TASK-92.41): whichever
/// goes first, the uploader, asking once more after, with no commit to come, has the commit's
/// segments, those filled and the one it ends in, which a closing between commits closes. A
/// framing of the segments filled that waited for a commit after, which may never come, would
/// leave the uploader without them (TASK-150.2, TASK-153.8).
fn uploading_what_a_spanning_commit_closed() {
    block_on(async {
        let dir = MemDir::new();
        let description = Description { block_size: BLOCK as u32, capacity: 1 << 20, cell: CELL };
        let cell = Arc::new(LocalCell::create(dir, description, ROLLING).unwrap());
        cell.write(extent(1), 0, &pattern(1, BLOCK)).await.unwrap();
        cell.commit(None).await.unwrap();
        let writer = {
            let cell = cell.clone();
            thread::spawn(move || {
                block_on(async move {
                    for sequence in 2..9 {
                        cell.write(extent(sequence), 0, &pattern(sequence as u8, 2 * BLOCK))
                            .await
                            .unwrap();
                    }
                    cell.commit_ordered(None).await.unwrap();
                })
            })
        };
        let mut taken = Vec::new();
        gather(&cell, &mut taken).await;
        writer.join().unwrap();
        gather(&cell, &mut taken).await;
        assert_eq!(taken.last(), Some(&2), "no commit's segment reached the uploader: {taken:?}");
    })
}

/// One run of a guest's requests while the flush of a commit, its record appended to the open
/// segment's log, stalls at its barrier, as on a slow disk: a flush holds none of the cell's locks
/// but its own as it makes commits durable (TASK-92.56, TASK-165.9). So the cell serves its
/// writes, reads and commits meanwhile; Shuttle, whose waits that can't end are deadlocks it
/// reports, finds so. Both commits are durable after, in their order.
fn serving_during_a_flush() {
    block_on(async {
        let dir = Stalling::new();
        let description = Description { block_size: BLOCK as u32, capacity: 1 << 20, cell: CELL };
        let cell = Arc::new(LocalCell::create(dir.clone(), description, OPTIONS).unwrap());
        cell.write(extent(1), 0, &pattern(1, BLOCK)).await.unwrap();
        dir.arm(Op::Barrier, "");
        let committing = {
            let cell = cell.clone();
            thread::spawn(move || block_on(async move { cell.commit(None).await.unwrap() }))
        };
        dir.wait_until_stalled();
        cell.write(extent(2), 0, &pattern(2, BLOCK)).await.unwrap();
        check_flushed(&cell, "a read while a flush syncs");
        cell.commit_ordered(None).await.unwrap();
        dir.let_go(Ok(()));
        committing.join().unwrap();
        cell.flush().await.unwrap();
        drop(cell);
        let reopened = LocalCell::open(dir, OPTIONS).unwrap();
        assert_eq!(reopened.last_commit().unwrap(), 2, "the commits made durable were not");
        check_flushed(&reopened, "the cell opened again");
    })
}

/// One run of a flush, an uploader's, of a commit only ordered, stalled at its barrier, as a
/// daemon's turn looks whether commits wait to be made durable (`LocalCell::flush_pending`): the
/// commit the flush took waits until it ends, not durable meanwhile. Pending only while no flush
/// had taken it, the daemon's turn, finding none, told the guest of it as durable, and a daemon
/// killed then lost it (GCE r717's crash-dax-daemon-killed, TASK-165.9).
fn pending_during_a_flush() {
    block_on(async {
        let dir = Stalling::new();
        let description = Description { block_size: BLOCK as u32, capacity: 1 << 20, cell: CELL };
        let cell = Arc::new(LocalCell::create(dir.clone(), description, OPTIONS).unwrap());
        cell.write(extent(1), 0, &pattern(1, BLOCK)).await.unwrap();
        cell.commit_ordered(None).await.unwrap();
        dir.arm(Op::Barrier, "");
        let flushing = {
            let cell = cell.clone();
            thread::spawn(move || block_on(async move { cell.flush().await.unwrap() }))
        };
        dir.wait_until_stalled();
        assert!(cell.flush_pending(), "a commit a flush in flight took is not pending");
        dir.let_go(Ok(()));
        flushing.join().unwrap();
        assert!(!cell.flush_pending(), "a commit made durable is pending");
    })
}

/// One run of a flush, as a daemon's turn makes its guests' commits durable, while the framing of
/// the segment a closing between commits, the uploader's, closed stalls, as on a slow disk: the
/// flush waits for no framing, its commits' records going to the logs of the segments they were
/// made in (TASK-165.9). Waiting for framings held the daemon's turns up past their 50 ms, and a
/// daemon killed meanwhile lost commits its guest was told were durable (GCE r719's
/// crash-dax-daemon-killed); Shuttle, whose waits that can't end are deadlocks it reports, finds a
/// flush that waits. A crash then recovers both commits, closing both segments again from their
/// logs, the one being framed and the open one.
fn flushing_during_a_framing() {
    block_on(async {
        let (dir, cell) = framing_stalls().await;
        let closing = spawn_closing(&cell);
        dir.wait_until_stalled();
        cell.write(extent(2), 0, &pattern(2, BLOCK)).await.unwrap();
        cell.commit_ordered(None).await.unwrap();
        cell.flush().await.unwrap();
        assert!(!cell.flush_pending(), "a commit flushed during a framing is pending");
        for seed in 0..4 {
            let crashed = LocalCell::open(dir.crash(seed), OPTIONS).unwrap();
            assert_eq!(
                crashed.last_commit().unwrap(),
                2,
                "a crash during a framing lost a commit flushed"
            );
            check_flushed(&crashed, "a crash during a framing");
        }
        dir.let_go(Ok(()));
        closing.join().unwrap();
    })
}

/// Reads extents 1 and 2 of a cell [`serving_during_a_flush`] wrote, through `cell`, which must
/// be as written.
fn check_flushed<D: Dir>(cell: &LocalCell<D>, failure: &str) {
    for sequence in 1..=2 {
        let mut read = vec![0; BLOCK];
        let reading = cell.read(extent(sequence), 0, &mut read);
        reading.unwrap_or_else(|error| panic!("{failure}: extent {sequence}: {error:#}"));
        assert!(read == pattern(sequence as u8, BLOCK), "{failure}: extent {sequence} differs");
    }
}

/// Shuttle's configuration: threads' stacks large enough for the cell's (Shuttle runs each on a
/// stack of its own, of 60 KiB by default).
fn config() -> Config {
    let mut config = Config::new();
    config.stack_size = 4 << 20;
    config
}

/// Runs random schedules of `test`.
fn randomly(test: fn()) {
    Runner::new(RandomScheduler::new(runs()), config()).run(test);
}

#[test]
fn flushes_in_flight_keep_their_commits_pending() {
    randomly(pending_during_a_flush);
}

#[test]
fn flushes_wait_for_no_framing() {
    randomly(flushing_during_a_framing);
}

#[test]
fn writer_closer_and_uploader_randomly() {
    set_rules(Rules::ALL);
    randomly(run);
}

#[test]
fn writer_closer_and_uploader_by_pct() {
    set_rules(Rules::ALL);
    Runner::new(PctScheduler::new(3, runs()), config()).run(run);
}

#[test]
fn writes_racing_commits_by_pct() {
    set_rules(Rules::ALL);
    Runner::new(PctScheduler::new(3, runs()), config()).run(racing);
}

#[test]
fn writes_filling_segments_by_pct() {
    set_rules(Rules::ALL);
    Runner::new(PctScheduler::new(3, runs()), config()).run(rolling_over);
}

#[test]
fn writes_filling_segments_randomly() {
    set_rules(Rules::ALL);
    randomly(rolling_over);
}

#[test]
fn frees_racing_writes_by_pct() {
    set_rules(Rules::ALL);
    Runner::new(PctScheduler::new(3, runs()), config()).run(freeing_during_a_write);
}

#[test]
fn uploads_begun_during_flushes_by_pct() {
    set_rules(Rules::ALL);
    Runner::new(PctScheduler::new(3, runs()), config()).run(uploading_during_flushes);
}

#[test]
fn failing_during_a_commit_by_pct() {
    set_rules(Rules::ALL);
    Runner::new(PctScheduler::new(3, runs()), config()).run(failing_during_a_commit);
}

#[test]
fn closing_during_a_commit_by_pct() {
    set_rules(Rules::ALL);
    Runner::new(PctScheduler::new(3, runs()), config()).run(closing_during_a_commit);
}

#[test]
fn closing_during_a_flush_by_pct() {
    set_rules(Rules::ALL);
    Runner::new(PctScheduler::new(3, runs()), config()).run(closing_during_a_flush);
}

#[test]
fn requests_served_during_a_framing_by_pct() {
    set_rules(Rules::ALL);
    Runner::new(PctScheduler::new(3, runs()), config()).run(serving_during_a_framing);
}

#[test]
fn closing_during_a_framing_by_pct() {
    set_rules(Rules::ALL);
    Runner::new(PctScheduler::new(3, runs()), config()).run(closing_during_a_framing);
}

#[test]
fn closing_after_a_failed_framing_by_pct() {
    set_rules(Rules::ALL);
    Runner::new(PctScheduler::new(3, runs()), config()).run(closing_after_a_failed_framing);
}

#[test]
fn requests_served_during_a_framing_at_a_commit_by_pct() {
    set_rules(Rules::ALL);
    Runner::new(PctScheduler::new(3, runs()), config()).run(serving_during_a_framing_at_a_commit);
}

#[test]
fn closing_during_a_framing_at_a_commit_by_pct() {
    set_rules(Rules::ALL);
    Runner::new(PctScheduler::new(3, runs()), config()).run(closing_during_a_framing_at_a_commit);
}

#[test]
fn uploads_of_what_a_segment_filled_holds_by_pct() {
    set_rules(Rules::ALL);
    let scheduler = PctScheduler::new(3, runs());
    Runner::new(scheduler, config()).run(uploading_what_a_segment_filled_holds);
}

#[test]
fn uploads_of_what_a_spanning_commit_closed_by_pct() {
    set_rules(Rules::ALL);
    let scheduler = PctScheduler::new(3, runs());
    Runner::new(scheduler, config()).run(uploading_what_a_spanning_commit_closed);
}

#[test]
fn requests_served_during_a_flush_by_pct() {
    set_rules(Rules::ALL);
    Runner::new(PctScheduler::new(3, runs()), config()).run(serving_during_a_flush);
}

/// Replays the schedule the file `SHUTTLE_REPLAY` names holds, as a failing run prints it, if set:
/// with this test's configuration, which `shuttle::replay`'s default stacks are too small for; and
/// with the rules `SHUTTLE_RULES_OFF` names (separated by commas) off, as a `needs_` test had them.
#[test]
fn replay() {
    let Some(path) = std::env::var_os("SHUTTLE_REPLAY") else { return };
    let mut rules = Rules::ALL;
    for rule in std::env::var("SHUTTLE_RULES_OFF").unwrap_or_default().split(',') {
        match rule.trim() {
            "" => {}
            "keep_frees" => rules.keep_frees = false,
            "close_waits_for_writers" => rules.close_waits_for_writers = false,
            "commit_waits_for_writes" => rules.commit_waits_for_writes = false,
            "recheck_after_waiting" => rules.recheck_after_waiting = false,
            "drop_writes_to_freed_extents" => rules.drop_writes_to_freed_extents = false,
            "close_waits_for_flushes" => rules.close_waits_for_flushes = false,
            "close_waits_for_framings" => rules.close_waits_for_framings = false,
            "framings_end_themselves" => rules.framings_end_themselves = false,
            other => panic!("SHUTTLE_RULES_OFF names no rule {other:?}"),
        }
    }
    set_rules(rules);
    let schedule = std::fs::read_to_string(path).unwrap();
    let scheduler = ReplayScheduler::new_from_encoded(schedule.trim());
    // The scenario it is of: `SHUTTLE_SCENARIO`, by its function's name.
    let scenario = match std::env::var("SHUTTLE_SCENARIO").as_deref() {
        Ok("racing") => racing,
        Ok("rolling_over") => rolling_over,
        Ok("freeing_during_a_write") => freeing_during_a_write,
        Ok("uploading_during_flushes") => uploading_during_flushes,
        Ok("closing_during_a_flush") => closing_during_a_flush,
        Ok("failing_during_a_commit") => failing_during_a_commit,
        Ok("closing_during_a_commit") => closing_during_a_commit,
        Ok("serving_during_a_framing") => serving_during_a_framing,
        Ok("flushing_during_a_framing") => flushing_during_a_framing,
        Ok("closing_during_a_framing") => closing_during_a_framing,
        Ok("closing_after_a_failed_framing") => closing_after_a_failed_framing,
        Ok("serving_during_a_framing_at_a_commit") => serving_during_a_framing_at_a_commit,
        Ok("closing_during_a_framing_at_a_commit") => closing_during_a_framing_at_a_commit,
        Ok("uploading_what_a_segment_filled_holds") => uploading_what_a_segment_filled_holds,
        Ok("uploading_what_a_spanning_commit_closed") => uploading_what_a_spanning_commit_closed,
        _ => run,
    };
    Runner::new(scheduler, config()).run(scenario);
}

/// The batches of schedules a `needs_` test runs at most to find its failure, by PCT and randomly
/// in turn.
const BATCHES: usize = 8;

/// Runs the schedules with `rules`, expecting one to fail with a panic saying one of `expected`: a
/// violation the rule turned off prevents, not another. A rule may prevent several, of which the
/// schedule decides which shows first. A failing schedule's first panic may poison locks, which
/// other threads then panic on: any of its panics may say it.
///
/// The schedules are random: a batch may find none that fails, which isn't that the rule is not
/// needed (TASK-55: one of 1,500 PCT schedules missed on Linux). Batches are run, by PCT and
/// randomly in turn, until one finds it, up to [`BATCHES`].
///
/// The panics are collected, and printed, by a hook installed for good, which Shuttle's chains
/// to: a failing run may leave the thread counted as panicking (its tasks' coroutines torn down
/// mid-panic), when the hook can't be set back. Each test is a process of its own (nextest).
fn fails_without(rules: Rules, expected: &[&str], test: fn()) {
    static PANICS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    std::panic::set_hook(Box::new(|info| {
        eprintln!("{info}");
        PANICS.lock().unwrap().push(info.payload_as_str().unwrap_or_default().to_owned());
    }));
    set_rules(rules);
    let failed = (0..BATCHES).any(|batch| {
        match std::panic::catch_unwind(|| {
            if batch % 2 == 0 {
                Runner::new(PctScheduler::new(3, runs()), config()).run(test);
            } else {
                Runner::new(RandomScheduler::new(runs()), config()).run(test);
            }
        }) {
            Ok(()) => false,
            // Its payload may hold Shuttle's objects (a lock's poisoned guard), which can't be
            // dropped outside a Shuttle run.
            Err(payload) => {
                std::mem::forget(payload);
                true
            }
        }
    });
    set_rules(Rules::ALL);
    let panics = std::mem::take(&mut *PANICS.lock().unwrap());
    assert!(failed, "no schedule failed with {rules:?}");
    assert!(
        panics.iter().any(|panic| expected.iter().any(|expected| panic.contains(expected))),
        "with {rules:?}, expected one of {expected:?}, got {panics:#?}"
    );
}

/// Frees not kept for the uploader: its read of an extent a commit wrote and a later one freed
/// finds no extent; or, taking the segments closed, it finds the file of extent 2, the one stored
/// on its own that the writer frees, gone.
#[test]
fn needs_frees_kept() {
    let expected = ["no extent", "extents/0000000000007/0000000000002 doesn't exist"];
    fails_without(Rules { keep_frees: false, ..Rules::ALL }, &expected, run);
}

#[test]
fn needs_closes_to_wait_for_writers() {
    let rules = Rules { close_waits_for_writers: false, ..Rules::ALL };
    // Caught by the uploader or the run's checks, whichever reads it first.
    fails_without(rules, &["other data than was written"], run);
}

/// Commits sealing extents writes in progress write to: what they write lands after the seal, in
/// a segment closed before the next commit names it, or lost from an extent's own file.
#[test]
fn needs_commits_to_wait_for_writes() {
    let rules = Rules { commit_waits_for_writes: false, ..Rules::ALL };
    fails_without(rules, &["a write raced a commit"], racing);
}

/// Commits that waited for the writes in progress going on over a cell that failed meanwhile.
#[test]
fn needs_rechecks_after_waiting_for_failures() {
    let rules = Rules { recheck_after_waiting: false, ..Rules::ALL };
    fails_without(rules, &["a commit went on over a cell that failed"], failing_during_a_commit);
}

/// Commits that waited for the writes in progress going on over a cell closed meanwhile, which
/// it may have been opened again since.
#[test]
fn needs_rechecks_after_waiting_for_closings() {
    let rules = Rules { recheck_after_waiting: false, ..Rules::ALL };
    fails_without(rules, &["a commit went on over a cell closed"], closing_during_a_commit);
}

/// Writes to extents freed since they began recorded as they finish: the extent holds the
/// write's data, or another placement's, which panics the cell, or is sealed outside the open
/// extents, which refuses the freer's write; or the cell's audit finds its state other than its
/// counts say, as the extent, freed, holds the write's data again (TASK-160.16).
#[test]
fn needs_writes_to_freed_extents_dropped() {
    let rules = Rules { drop_writes_to_freed_extents: false, ..Rules::ALL };
    let expected = [
        "a write to a freed extent landed",
        "an extent's data is in segments or its own file",
        local_cell::AUDIT_FAILED,
    ];
    fails_without(rules, &expected, freeing_during_a_write);
}

/// The cell closing while a flush is in flight, as if nothing were left to flush: a crash then
/// loses commits made before it closed.
#[test]
fn needs_closes_to_wait_for_flushes() {
    let rules = Rules { close_waits_for_flushes: false, ..Rules::ALL };
    fails_without(rules, &["a crash after the cell closed"], closing_during_a_flush);
}

/// The cell closing while a closing between commits frames a segment: another opening
/// of the cell frames it again meanwhile, from its raw file, which the first removes, or the first
/// finds the framed file made.
#[test]
fn needs_closes_to_wait_for_framings() {
    let rules = Rules { close_waits_for_framings: false, ..Rules::ALL };
    fails_without(rules, &["raced the cell's closing"], closing_during_a_framing);
}

/// The segments a commit closes past the one it ends in, those filled since the commit before,
/// framed only by the next commit, as its end was driven by what came after it (TASK-150.2,
/// TASK-92.41): an uploader that waits for the commit waits for as long as no commit comes. A
/// framing that ends itself ends whatever comes (TASK-153.8).
#[test]
fn needs_framings_to_end_themselves() {
    let rules = Rules { framings_end_themselves: false, ..Rules::ALL };
    fails_without(rules, &["no commit's segment"], uploading_what_a_spanning_commit_closed);
}
