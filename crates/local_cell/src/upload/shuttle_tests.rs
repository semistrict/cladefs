// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a cell committing, sealing and forking, its uploader uploading what it commits, and
//! passes of the collector running over the bucket all along (TASK-291.1), under Shuttle, whose
//! scheduler chooses every interleaving of their threads at each of the cell's locks and at each
//! request to the bucket, and replays a failing one from its seed.
//! `cargo nextest run -p local_cell --features shuttle --lib shuttle_tests::`.

use super::*;
use crate::{Description, MemDir};
use bucket_cell::bucket::{ByteRange, Error as BucketError, Meta, Object, PutMode, Version};
use bucket_cell::collector;
use bucket_cell::fetch::Limits;
use bucket_cell::testing::{Direct, SimBucket};
use shuttle::scheduler::{PctScheduler, RandomScheduler, Scheduler};
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::Ordering::SeqCst;
use storage_device::ExtentId;

const BLOCK: usize = 4096;
const CELL: bucket_cell::layout::CellId = bucket_cell::layout::test_cell(7);

/// Small segments, so that commits close several, and extents of two blocks or more in
/// objects of their own, which upload before the segments naming them (TASK-79.3); flushes of
/// two blocks at most held in their log records, larger ones sealed (TASK-301).
const OPTIONS: crate::Options = crate::Options {
    max_extent: 64 << 20,
    segment_size: 3 * BLOCK as u64,
    segment_age: Duration::from_secs(3600),
    large_extent: 2 * BLOCK as u64,
    open_files: 2,
    held_most: 2 * BLOCK as u64,
};

/// The schedules each test runs, randomly and by PCT; `SHUTTLE_SOAK` multiplies them.
fn runs() -> usize {
    let soak: usize = std::env::var("SHUTTLE_SOAK").map_or(1, |soak| soak.parse().unwrap());
    200 * soak
}

/// A bucket in memory whose requests each give Shuttle's scheduler a turn first, as a real
/// bucket's round trips let the other threads go on: so that a pass of the collector, the
/// uploader and the cell interleave at each. Its requests take effect at once, and never fail.
/// Its gate, once armed, holds up the next listing of the whole bucket, a pass's of its
/// objects.
struct Turns {
    direct: Direct,
    gate: Gate,
}

impl Turns {
    fn new() -> Arc<Self> {
        Arc::new(Self { direct: Direct(SimBucket::new()), gate: Gate::default() })
    }
}

/// Holds up a listing of a whole cell, a pass's of its candidates (TASK-395.4), once armed,
/// until opened.
#[derive(Default)]
struct Gate {
    armed: shuttle::sync::atomic::AtomicBool,
    reached: shuttle::sync::atomic::AtomicBool,
    open: shuttle::sync::atomic::AtomicBool,
}

impl Gate {
    fn arm(&self) {
        self.armed.store(true, SeqCst);
    }

    /// Waits until a listing held up reaches the gate.
    fn reached(&self) {
        until(|| self.reached.load(SeqCst));
    }

    fn open(&self) {
        self.open.store(true, SeqCst);
    }

    /// A listing of `prefix` goes through the gate.
    fn pass(&self, prefix: &str) {
        let whole = bucket_cell::layout::cell_of_prefix(prefix).is_some();
        if whole && self.armed.swap(false, SeqCst) {
            self.reached.store(true, SeqCst);
            until(|| self.open.load(SeqCst));
        }
    }
}

impl bucket_cell::Bucket for Turns {
    fn get(&self, key: &str) -> impl Future<Output = Result<Object, BucketError>> + Send {
        shuttle::thread::yield_now();
        self.direct.get(key)
    }

    fn get_range(
        &self,
        key: &str,
        range: ByteRange,
    ) -> impl Future<Output = Result<Vec<u8>, BucketError>> + Send {
        shuttle::thread::yield_now();
        self.direct.get_range(key, range)
    }

    fn head(&self, key: &str) -> impl Future<Output = Result<Meta, BucketError>> + Send {
        shuttle::thread::yield_now();
        self.direct.head(key)
    }

    fn list(&self, prefix: &str) -> impl Future<Output = Result<Vec<Meta>, BucketError>> + Send {
        shuttle::thread::yield_now();
        self.gate.pass(prefix);
        self.direct.list(prefix)
    }

    fn list_delimited(
        &self,
        prefix: &str,
        page: bucket_cell::bucket::Page<'_>,
    ) -> impl Future<Output = Result<bucket_cell::bucket::Listing, BucketError>> + Send {
        self.direct.list_delimited(prefix, page)
    }

    fn put(
        &self,
        key: &str,
        bytes: bucket_cell::Bytes,
        mode: PutMode,
    ) -> impl Future<Output = Result<Version, BucketError>> + Send {
        shuttle::thread::yield_now();
        self.direct.put(key, bytes, mode)
    }

    fn delete(&self, key: &str) -> impl Future<Output = Result<(), BucketError>> + Send {
        shuttle::thread::yield_now();
        self.direct.delete(key)
    }

    fn now(&self) -> u64 {
        self.direct.now()
    }
}

/// The bytes of the extent at `at`, `blocks` of them.
fn pattern(at: u64, blocks: usize) -> Vec<u8> {
    (0..blocks * BLOCK).map(|i| (i % 251) as u8 ^ at as u8).collect()
}

/// Waits, giving the scheduler turns, for what `receiver` gets: a thread of Shuttle's blocking
/// on the channel would hold up every other.
fn received<T>(receiver: &mut futures::channel::oneshot::Receiver<T>) -> T {
    loop {
        match receiver.try_recv() {
            Ok(Some(received)) => return received,
            Ok(None) => shuttle::thread::yield_now(),
            Err(err) => panic!("{err}"),
        }
    }
}

/// What the cell `cell`, recovered from `bucket`, holds, if not `holds`, extent by extent.
fn differs(
    bucket: &Arc<Turns>,
    cell: CellId,
    holds: &BTreeMap<ExtentId, Vec<u8>>,
) -> Result<(), String> {
    let failed = |err: bucket_cell::Error| format!("cell {cell}: {err}");
    let record = shuttle::future::block_on(bucket_cell::head::get(&**bucket, cell));
    let record = record.map_err(failed)?.ok_or(format!("cell {cell}: no head"))?;
    let recovering = bucket_cell::recover(&**bucket, cell, record.origin());
    let state = shuttle::future::block_on(recovering).map_err(failed)?.state;
    let held: Vec<&ExtentId> = state.extents.keys().collect();
    if held != holds.keys().collect::<Vec<_>>() {
        return Err(format!("cell {cell} holds {held:?}, not {:?}", holds.keys()));
    }
    for (extent, location) in &state.extents {
        let read = shuttle::future::block_on(bucket_cell::testing::read(bucket, *extent, location));
        if read.as_ref() != Ok(&holds[extent]) {
            return Err(format!("cell {cell}: {extent:?} reads {read:?}"));
        }
    }
    Ok(())
}

/// Waits, giving the scheduler turns, until `done`.
fn until(done: impl Fn() -> bool) {
    while !done() {
        shuttle::thread::yield_now();
    }
}

/// A cell of `cell` on a disk of its own, made empty, and its uploader, running on a thread
/// of its own as `holder`, whose writer has taken the cell.
struct Session {
    uploader: Arc<Shared<MemDir, Turns>>,
    running: shuttle::thread::JoinHandle<()>,
    open_cell: Arc<LocalCell<MemDir>>,
    cell: CellId,
    /// What the cell's extents hold, by their IDs.
    holds: BTreeMap<ExtentId, Vec<u8>>,
    /// The first sequence number of its own extents'.
    next: u64,
}

impl Session {
    fn open(bucket: &Arc<Turns>, cell: CellId, holder: u64) -> Self {
        let dir = MemDir::new();
        let description = Description { block_size: BLOCK as u32, capacity: 1 << 30, cell };
        LocalCell::make(&dir, description, None, None).unwrap();
        shuttle::future::block_on(lifecycle::create(&**bucket, cell, holder)).unwrap();
        let recovering = LocalCell::recover_unmade(dir).unwrap();
        // Its interval never runs out: the writer asks for what it commits.
        let never: Clock = Arc::new(|_| futures::future::pending().boxed());
        let spawn: Spawn = Arc::new(|task| drop(shuttle::future::spawn(task)));
        let options = Options { token: holder, ..Options::default() };
        let uploader = Shared::new(
            options,
            Budget::new(Limits::UNBOUNDED),
            (never, spawn),
            bucket_cell::keys::testing::host(),
        );
        let (started, mut start) = futures::channel::oneshot::channel();
        let running = {
            let (uploader, bucket) = (uploader.clone(), bucket.clone());
            shuttle::thread::spawn(move || {
                let running = uploader.run(recovering, OPTIONS, bucket, holder, started, None);
                shuttle::future::block_on(running)
            })
        };
        let open_cell = received(&mut start).unwrap();
        open_cell.made().unwrap();
        let next = uploader.next_sequence();
        Self { uploader, running, open_cell, cell, holds: BTreeMap::new(), next }
    }

    fn extent(&self, at: u64) -> ExtentId {
        ExtentId::new(self.cell.id, self.next + at)
    }

    fn write(&mut self, at: u64, blocks: usize) {
        let bytes = pattern(self.cell.id ^ at, blocks);
        shuttle::future::block_on(self.open_cell.write(self.extent(at), 0, &bytes)).unwrap();
        self.holds.insert(self.extent(at), bytes);
    }

    fn free(&mut self, extent: ExtentId) {
        self.open_cell.free(extent).unwrap();
        self.holds.remove(&extent);
    }

    /// Asks the uploader for what the cell committed.
    fn ask(&self) {
        self.uploader.ask(self.open_cell.last_commit().unwrap(), Awaited::Uploaded).unwrap();
    }

    /// The uploader uploads all the cell made durable, and stops; and the cell closes.
    fn end(self) -> Result<BTreeMap<ExtentId, Vec<u8>>, String> {
        let last = self.open_cell.last_commit().unwrap();
        self.uploader.stop();
        self.running.join().unwrap();
        drop(self.open_cell);
        let (failed, uploaded) = (self.uploader.failed(), self.uploader.last_uploaded());
        if failed != Some(Failure::msg("the uploader stopped")) || uploaded != last {
            return Err(format!("the uploader: {failed:?}, through {uploaded} of {last}"));
        }
        Ok(self.holds)
    }
}

/// One schedule: the writer writes, frees and commits, asking for uploads; its uploader
/// uploads; passes of the collector run all along. Then the uploader uploads what is left and
/// stops, a last pass runs alone, and the cell, recovered from the bucket, holds what its
/// cell's last commit left, each extent reading as it was written. What went wrong, if anything
/// did.
fn collected_while_committing() -> Result<(), String> {
    let bucket = Turns::new();
    let mut parent = Session::open(&bucket, CELL, 1);
    // Passes, one after another, for as long as the writers write and their uploads go.
    let writing = Arc::new(shuttle::sync::atomic::AtomicBool::new(true));
    let collecting = {
        let (bucket, writing) = (bucket.clone(), writing.clone());
        shuttle::thread::spawn(move || {
            for holder in 100.. {
                let pass = collector::collect(&*bucket, holder);
                shuttle::future::block_on(pass).map_err(|err| format!("pass {holder}: {err}"))?;
                if !writing.load(SeqCst) {
                    return Ok::<_, String>(());
                }
            }
            unreachable!("passes for ever")
        })
    };

    parent.write(0, 1);
    shuttle::future::block_on(parent.open_cell.commit_ordered(None)).unwrap();
    // Asked for that commit with an extent of its own open, the uploader closes the open
    // segment between commits: its footer's next sequence number counts the extent, which the
    // next segment names, its object uploaded first (TASK-79.3).
    parent.write(1, 3);
    parent.ask();
    parent.write(2, 2);
    parent.free(parent.extent(0));
    shuttle::future::block_on(parent.open_cell.commit(None)).unwrap();
    parent.ask();
    parent.write(3, 1);
    shuttle::future::block_on(parent.open_cell.commit(None)).unwrap();
    parent.ask();
    parent.free(parent.extent(1));
    parent.write(4, 2);
    shuttle::future::block_on(parent.open_cell.commit_ordered(None)).unwrap();
    parent.ask();
    parent.free(parent.extent(3));
    shuttle::future::block_on(parent.open_cell.commit(None)).unwrap();
    parent.ask();
    let parent = parent.end()?;
    writing.store(false, SeqCst);
    collecting.join().unwrap()?;
    let pass = collector::collect(&*bucket, 200);
    shuttle::future::block_on(pass).map_err(|err| format!("the last pass: {err}"))?;
    differs(&bucket, CELL, &parent)
}

/// Runs `schedule` under `scheduler`'s interleavings.
fn explore(scheduler: impl Scheduler + 'static, schedule: impl Fn() + Send + Sync + 'static) {
    let mut config = shuttle::Config::new();
    config.stack_size = 4 << 20;
    shuttle::Runner::new(scheduler, config).run(schedule);
}

/// TASK-291.1: whatever the interleaving, what the cell committed and its uploader
/// acknowledged stays its cell's, the collector running all along. [`listed_while_committing`]
/// holds a pass up as it lists.
#[test]
fn collection_keeps_what_a_cell_commits_meanwhile() {
    let schedule = || collected_while_committing().unwrap();
    explore(RandomScheduler::new(runs()), schedule);
    explore(PctScheduler::new(3, runs()), schedule);
}

/// One schedule: a pass of the collector, held up as it lists the cell's objects, while the
/// cell commits and its uploader uploads its segment and acknowledges it. Then the pass goes
/// on, the uploader uploads what is left and stops, a last pass runs alone, and the cell,
/// recovered from the bucket, holds what its cell's last commit left. What went wrong, if
/// anything did.
fn listed_while_committing() -> Result<(), String> {
    let bucket = Turns::new();
    let mut parent = Session::open(&bucket, CELL, 1);
    parent.write(0, 1);
    parent.write(1, 3);
    shuttle::future::block_on(parent.open_cell.commit_ordered(None)).unwrap();
    parent.ask();
    let before = parent.open_cell.last_commit().unwrap();
    until(|| parent.uploader.last_uploaded() >= before);
    bucket.gate.arm();
    let passing = {
        let bucket = bucket.clone();
        shuttle::thread::spawn(move || {
            let pass = collector::collect(&*bucket, 100);
            shuttle::future::block_on(pass).map_err(|err| format!("the pass: {err}"))
        })
    };
    bucket.gate.reached();
    parent.write(2, 1);
    parent.free(parent.extent(0));
    shuttle::future::block_on(parent.open_cell.commit_ordered(None)).unwrap();
    parent.ask();
    let committed = parent.open_cell.last_commit().unwrap();
    until(|| parent.uploader.last_uploaded() >= committed);
    bucket.gate.open();
    passing.join().unwrap()?;
    let parent = parent.end()?;
    let pass = collector::collect(&*bucket, 200);
    shuttle::future::block_on(pass).map_err(|err| format!("the last pass: {err}"))?;
    differs(&bucket, CELL, &parent)
}

/// A pass listing the cell's objects before it reads its head keeps what the writer uploads
/// and acknowledges after the listing began: none of it is a candidate.
#[test]
fn a_pass_keeps_what_a_writer_uploads_as_it_lists() {
    explore(RandomScheduler::new(runs() / 10), || listed_while_committing().unwrap());
}
