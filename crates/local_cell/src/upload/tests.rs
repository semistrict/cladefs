// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;
use crate::{Description, MemDir, Sparse};
use bucket_cell::fetch::Limits;
use storage_device::ExtentId;

const CELL: bucket_cell::layout::CellId = bucket_cell::layout::test_cell(7);

/// TASK-449's review: a failed making of the bucket runtime, a thread not spawned as under a
/// limit of the process's threads, fails the call it was made for, not every call after for
/// the process's life: every fork, take and deletion of a host's until its daemon restarted.
/// The next call makes it anew. Its own process, as nextest runs each test, whose first call
/// makes the runtime.
#[test]
fn calls_after_a_failed_making_of_the_runtime_make_it_anew() {
    let scenario = fail::FailScenario::setup();
    fail::cfg("upload::bucket_runtime", "return").unwrap();
    let failed = block_on(async { 7 }).err().map(|err| err.to_string());
    fail::remove("upload::bucket_runtime");
    assert_eq!(failed.as_deref(), Some("failpoint upload::bucket_runtime"));
    assert_eq!(block_on(async { 7 }).ok(), Some(7));
    assert_eq!(block_on(async { 8 }).ok(), Some(8));
    scenario.teardown();
}

/// TASK-449's review: what a call from no runtime's task spawns, a bucket's connection as it
/// connects, runs on the bucket runtime, which the host's uploaders and fetches run on: so
/// the connections such calls leave in the bucket's pool, which the uploads take next, are
/// served by its threads, not by a runtime of the calls' own beside it, of fewer.
#[test]
fn calls_from_no_task_spawn_on_the_bucket_runtime() {
    let spawned = block_on(async {
        tokio::spawn(async { std::thread::current().name().map(str::to_owned) }).await.unwrap()
    });
    assert_eq!(spawned.unwrap().as_deref(), Some("cladefs bucket"));
}

/// TASK-92.52: a stall says what it waits for: the bytes past the lag bound, which a stall over
/// it waits on, and the commits not uploaded and the oldest's age, which a slow upload shows.
#[test]
fn a_stall_says_what_it_waits_for() {
    let over = stalled(5 << 20, 4 << 20, 3, Some(Duration::from_millis(250)));
    assert_eq!(
        over,
        "cladefs: commits wait for object storage: 5242880 bytes not uploaded, 1048576 past the \
         lag bound of 4194304; 3 commits not uploaded, the oldest seen 250 ms ago"
    );
    let slow = stalled(1 << 20, 4 << 20, 1, None);
    assert_eq!(
        slow,
        "cladefs: commits wait for object storage: 1048576 bytes not uploaded, 0 past the lag \
         bound of 4194304; 1 commits not uploaded, none seen yet"
    );
}

/// TASK-364: an opening takes its cell, making it for a new cell, whose cell it is from then on
/// (doc-20); its cell's head is taken with the cell's epoch. A later placement's taking of the
/// cell, and of the cell's head with the epoch it took it at, fences the cell's writer: its
/// next commit's upload fails, and a placement of a lower token takes the cell back no more.
#[tokio::test(flavor = "multi_thread")]
async fn open_cells_take_their_cells_and_are_fenced_with_them() {
    let host = Runtime {
        handle: tokio::runtime::Handle::current(),
        uploads: Budget::new(Limits::UNBOUNDED),
        keys: bucket_cell::keys::testing::host(),
    };
    let bucket =
        Arc::new(ObjectBucket::new(Arc::new(object_store::memory::InMemory::new()), "").unrouted());
    let dir = MemDir::new();
    let description = Description { block_size: 4096, capacity: 1 << 30, cell: CELL };
    let kind = bucket_cell::cell::Kind::Fxfs;
    LocalCell::make(&dir, description, Some("memory:///cells"), Some(kind)).unwrap();
    lifecycle::create(&*bucket, CELL, 1).await.unwrap();
    let options = Options { token: 5, ..Options::default() };
    let recovering = LocalCell::recover_unmade(dir).unwrap();
    let (cell, uploader) =
        open(recovering, crate::Options::default(), bucket.clone(), (1, options), &host)
            .await
            .unwrap();
    cell.made().unwrap();
    let head = bucket_cell::head::get(&*bucket, CELL).await.unwrap().unwrap();
    assert_eq!((head.epoch, head.holder, head.token), (1, 1, 5));
    assert_eq!(head.kind, Some(bucket_cell::cell::Kind::Fxfs));

    cell.write(ExtentId::new(CELL.id, 1), 0, &[1; 4096]).await.unwrap();
    cell.commit(None).await.unwrap();
    uploader.uploaded(cell.last_commit().unwrap()).await.unwrap();
    // Another placement takes the cell, fencing it (TASK-371).
    let taken = bucket_cell::testing::take(&*bucket, CELL, 0x66, 6).await;
    assert_eq!(taken.map(|held| held.head.epoch), Ok(2));
    cell.write(ExtentId::new(CELL.id, 2), 0, &[2; 4096]).await.unwrap();
    cell.commit(None).await.unwrap();
    let fenced = uploader.uploaded(cell.last_commit().unwrap()).await;
    assert!(matches!(fenced, Err(Failure::Bucket(bucket_cell::Error::Fenced))), "{fenced:?}");
    let back = bucket_cell::testing::take(&*bucket, CELL, 0x55, 5).await;
    assert_eq!(back.map(|held| held.head), Err(bucket_cell::Error::Stale));
}

/// cladefs: the blocks of the cells of [`Doc27`].
const BLOCK: usize = 4096;

/// cladefs: a cell made in a bucket in memory, opened with an uploader of `options`, its
/// extents of a block each, each segment of it uploaded as it is committed: doc-27's tests'
/// (board 6214).
struct Doc27 {
    host: Runtime,
    bucket: Arc<RoutedBucket>,
    dir: MemDir,
    cell: Arc<LocalCell<MemDir>>,
    uploader: Uploader<MemDir>,
}

impl Doc27 {
    async fn open(options: Options) -> Self {
        let host = Runtime {
            handle: tokio::runtime::Handle::current(),
            uploads: Budget::new(Limits::UNBOUNDED),
            keys: bucket_cell::keys::testing::host(),
        };
        let memory = object_store::memory::InMemory::new();
        let bucket = Arc::new(ObjectBucket::new(Arc::new(memory), "").unrouted());
        let dir = MemDir::new();
        let description = Description { block_size: BLOCK as u32, capacity: 1 << 30, cell: CELL };
        LocalCell::make(&dir, description, Some("memory:///doc-27"), None).unwrap();
        lifecycle::create(&*bucket, CELL, 1).await.unwrap();
        let recovering = LocalCell::recover_unmade(dir.clone()).unwrap();
        let opening = (1, Options { token: 5, ..options });
        let (cell, uploader) =
            open(recovering, crate::Options::default(), bucket.clone(), opening, &host)
                .await
                .unwrap();
        cell.made().unwrap();
        Self { host, bucket, dir, cell, uploader }
    }

    /// Opens the cell again, on what its bucket holds, by another holder: its uploader cut
    /// short first, writing no checkpoint.
    async fn reopen(self, options: Options) -> Self {
        let Self { host, bucket, dir, cell, uploader } = self;
        uploader.cut_short()();
        uploader.close().await;
        cell.close().await.unwrap();
        drop(cell);
        let recovering = LocalCell::recover(dir.clone()).unwrap();
        let opening = (2, Options { token: 6, ..options });
        let (cell, uploader) =
            open(recovering, crate::Options::default(), bucket.clone(), opening, &host)
                .await
                .unwrap();
        Self { host, bucket, dir, cell, uploader }
    }

    fn extent(sequence: u64) -> ExtentId {
        ExtentId::new(CELL.id, sequence)
    }

    /// A segment of its own, uploaded: the extents `write` writes, each a block of its
    /// number, and the frees of `free`.
    async fn segment(&self, write: std::ops::Range<u64>, free: &[u64]) {
        for sequence in write {
            let data = [sequence as u8; BLOCK];
            self.cell.write(Self::extent(sequence), 0, &data).await.unwrap();
        }
        for &sequence in free {
            self.cell.free(Self::extent(sequence)).unwrap();
        }
        self.cell.commit(None).await.unwrap();
        self.uploader.uploaded(self.cell.last_commit().unwrap()).await.unwrap();
    }

    /// Waits until `holds` holds of the cell.
    async fn until(&self, what: &str, holds: impl Fn(&Self) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !holds(self) {
            assert!(std::time::Instant::now() < deadline, "never {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The head's checkpoint now.
    async fn checkpoint(&self) -> Option<(u64, u64)> {
        bucket_cell::head::get(&*self.bucket, CELL).await.unwrap().unwrap().checkpoint
    }

    /// Where `sequence`'s extent is in the bucket, if it is there.
    fn located(&self, sequence: u64) -> Option<bucket_cell::layout::Location> {
        match self.cell.place(Self::extent(sequence)).unwrap() {
            Some(crate::Place::Bucket(at)) => Some(at),
            _ => None,
        }
    }

    /// The key of the object `sequence`'s extent is in, in the bucket, if it is there.
    fn in_bucket(&self, sequence: u64) -> Option<String> {
        self.located(sequence).map(|at| at.key)
    }

    /// `sequence`'s extent's bytes, read where the cell holds it in the bucket, under its
    /// writer's keys.
    async fn read_in_bucket(&self, sequence: u64) -> Vec<u8> {
        let at = self.located(sequence).expect("in the bucket");
        let keys = self.uploader.shared.writer().unwrap().keys().clone();
        let extent = Self::extent(sequence);
        bucket_cell::fetch::read(&self.bucket, &keys, extent, &at).await.unwrap()
    }
}

/// cladefs: segment `segment` of [`CELL`]'s first writer's.
fn segment_key(segment: u64) -> String {
    bucket_cell::layout::segment(CELL, segment, 1)
}

/// cladefs: doc-27 (board 6214): a cell opened again on what its bucket holds knows the length
/// of each of its segments' data, with no request of its own: those a checkpoint names, kept
/// in it, and those after it, from the trailers its recovery reads their footers by. So it
/// finds the segments sparse that the cell before it found, each its data's length as framed,
/// which its acknowledgements took from the write-ahead log's account of it.
#[tokio::test(flavor = "multi_thread")]
async fn cells_opened_again_know_their_segments_lengths() {
    let hour = Duration::from_secs(3600);
    let options = Options { interval: hour, checkpoint_every: 3, ..Options::default() };
    let cell = Doc27::open(options.clone()).await;
    cell.segment(0..8, &[]).await;
    cell.segment(8..16, &[]).await;
    // The second freed whole: its length goes.
    cell.segment(16..17, &(8..16).collect::<Vec<_>>()).await;
    // The checkpoint of those three landed, before what comes after.
    let mut checkpoint = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while checkpoint.is_none() {
        assert!(std::time::Instant::now() < deadline, "no checkpoint written");
        tokio::time::sleep(Duration::from_millis(10)).await;
        checkpoint = cell.checkpoint().await;
    }
    assert_eq!(checkpoint, Some((2, 1)), "the checkpoint of the first three");
    let block = BLOCK as u64;
    let key = bucket_cell::layout::checkpoint(CELL, 2, 1);
    let checkpoint = bucket_cell::bucket::get(&*cell.bucket, &key).await.unwrap().unwrap();
    let lengths = bucket_cell::checkpoint::decode(&checkpoint.bytes).unwrap().lengths;
    let named = [(segment_key(0), 8 * block), (segment_key(2), block)];
    assert_eq!(lengths, named.into_iter().collect(), "the checkpoint's lengths");
    cell.segment(17..21, &[]).await;
    cell.segment(21..21, &[0, 1, 2, 3, 4, 17, 18, 19]).await;
    let sparse = [
        Sparse { key: segment_key(0), live: 3 * block, len: 8 * block },
        Sparse { key: segment_key(3), live: block, len: 4 * block },
    ];
    assert_eq!(cell.cell.sparse(|_| false).unwrap(), sparse, "as acknowledged");
    // Gone with no checkpoint more: what follows the first is read by its trailers.
    let cell = cell.reopen(options).await;
    let head = bucket_cell::head::get(&*cell.bucket, CELL).await.unwrap().unwrap();
    assert_eq!((head.checkpoint, head.end), (Some((2, 1)), 5), "no checkpoint more");
    assert_eq!(cell.cell.sparse(|_| false).unwrap(), sparse, "as recovered");
}

/// cladefs: doc-27 (board 6214): a relocation round copies the extents a cell holds still in
/// its sparse segments into its open segment, sparsest first, within what it is allowed, and
/// nothing of one a version the head names reads, which keeps it whole: once the version is
/// dropped, the next round relocates out of it. Its extents read as they were, where they went
/// once uploaded, and so does a cell opened on what the bucket holds; the segment they were in
/// holds nothing of the cell's then, for the collector to delete.
#[tokio::test(flavor = "multi_thread")]
async fn rounds_relocate_out_of_sparse_segments_no_kept_state_reads() {
    let hour = Duration::from_secs(3600);
    let options = Options { interval: hour, relocate: false, ..Options::default() };
    let cell = Doc27::open(options.clone()).await;
    let block = BLOCK as u64;
    cell.segment(0..8, &[]).await;
    let version = extent_frame::segment::NewVersion { id: 1, keep_last: None };
    cell.uploader.uploaded(cell.cell.commit_version(version, false).await.unwrap()).await.unwrap();
    cell.segment(8..9, &[0, 1, 2, 3, 4, 5]).await;
    let writer = cell.uploader.shared.writer().unwrap();
    let pins = relocations::Pins::default();
    let round = async |allowance| {
        // As a host's: its fetchers' budget and memory (`Options::fetching`).
        let fetching = Some(Fetching {
            budgets: Budget::new(Limits::UNBOUNDED).into(),
            memory: bucket_cell::fetch::Memory::new(),
        });
        let budgets = relocations::Budgets { uploads: &cell.host.uploads, fetching, cache: None };
        let places = (&cell.bucket, budgets);
        relocations::round(&cell.cell, &writer, places, &pins, allowance).await.unwrap().bytes
    };
    assert_eq!(round(u64::MAX).await, 0, "its version keeps the first segment whole");
    cell.uploader.drop_versions(&BTreeSet::from([1])).await.unwrap();
    assert_eq!(round(block).await, block, "a block allowed");
    assert_eq!(round(u64::MAX).await, block, "the rest");
    assert_eq!(round(u64::MAX).await, 0, "nothing sparse");
    cell.uploader.uploaded(cell.cell.last_commit().unwrap()).await.unwrap();
    for sequence in [6, 7] {
        let key = cell.in_bucket(sequence).expect("in the bucket");
        assert!(key != segment_key(0) && key.starts_with(&bucket_cell::layout::segments(CELL)));
        assert_eq!(cell.read_in_bucket(sequence).await, [sequence as u8; BLOCK]);
    }
    let cell = cell.reopen(options).await;
    for sequence in [6, 7, 8] {
        assert_ne!(cell.in_bucket(sequence), Some(segment_key(0)));
        let data = cell.read_in_bucket(sequence).await;
        assert_eq!(data, [sequence as u8; BLOCK], "{sequence}, opened again");
    }
}

/// cladefs: doc-27 (board 6214): a writer relocates at its checkpoint rounds unasked: a
/// segment mostly freed holds nothing of the cell's once the round after its frees' segment's
/// checkpoint is done.
#[tokio::test(flavor = "multi_thread")]
async fn writers_relocate_at_their_checkpoint_rounds() {
    let hour = Duration::from_secs(3600);
    let cell =
        Doc27::open(Options { interval: hour, checkpoint_every: 2, ..Options::default() }).await;
    cell.segment(0..8, &[]).await;
    cell.segment(8..9, &[0, 1, 2, 3, 4, 5]).await;
    let moved = |cell: &Doc27| [6, 7].iter().all(|&sequence| cell.in_bucket(sequence).is_none());
    cell.until("relocated", moved).await;
    cell.uploader.uploaded(cell.cell.last_commit().unwrap()).await.unwrap();
    for sequence in [6, 7] {
        assert_ne!(cell.in_bucket(sequence), Some(segment_key(0)), "{sequence}");
    }
    assert_eq!(cell.cell.sparse(|_| false).unwrap(), []);
}

/// cladefs: doc-27 (board 6214, AC #2): a cell freeing and writing at random holds less than
/// twice its live bytes in the bucket once its writer's rounds relocated out of its sparse
/// segments and the collector, which compacts nothing, deleted what no state reads: no segment
/// left under half live ([`LocalCell::sparse`]). Without relocations it held three times as
/// much by the end, its first segment never freed whole.
#[tokio::test(flavor = "multi_thread")]
async fn cells_freeing_and_writing_at_random_hold_under_twice_their_live_bytes() {
    use rand::{RngExt as _, SeedableRng as _};
    // Extents live, of a block each; freed and written at random each round, in a segment.
    const LIVE: u64 = 256;
    const CHURN: usize = 32;
    const ROUNDS: usize = 16;
    let interval = Duration::from_millis(20);
    let cell = Doc27::open(Options { interval, checkpoint_every: 1, ..Options::default() }).await;
    let mut rng = rand::rngs::StdRng::seed_from_u64(0x6214);
    // Noise of its number, so that what the bucket holds is what the segments' data says,
    // not compressed.
    let noise = |sequence: u64| -> Vec<u8> {
        let mut rng = rand::rngs::StdRng::seed_from_u64(sequence);
        (0..BLOCK).map(|_| rng.random()).collect()
    };
    let write = async |sequences: std::ops::Range<u64>| {
        for sequence in sequences {
            cell.cell.write(Doc27::extent(sequence), 0, &noise(sequence)).await.unwrap();
        }
    };
    write(0..LIVE).await;
    cell.cell.commit(None).await.unwrap();
    let mut live: Vec<u64> = (0..LIVE).collect();
    let mut next = LIVE;
    let prefix = CELL.prefix();
    for round in 0..ROUNDS {
        for _ in 0..CHURN {
            let freed = live.swap_remove(rng.random_range(0..live.len()));
            cell.cell.free(Doc27::extent(freed)).unwrap();
        }
        write(next..next + CHURN as u64).await;
        live.extend(next..next + CHURN as u64);
        next += CHURN as u64;
        cell.cell.commit(None).await.unwrap();
        cell.uploader.uploaded(cell.cell.last_commit().unwrap()).await.unwrap();
        // The rounds relocated what the frees left sparse, and a checkpoint names where.
        cell.until("relocated", |cell| cell.cell.sparse(|_| false).unwrap().is_empty()).await;
        cell.uploader.uploaded(cell.cell.last_commit().unwrap()).await.unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let head = bucket_cell::head::get(&*cell.bucket, CELL).await.unwrap().unwrap();
            if head.checkpoint.is_some_and(|(at, _)| at + 1 == head.end) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "round {round}: no checkpoint");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        bucket_cell::testing::collect_until_quiet(&*cell.bucket).await.unwrap();
        let listed = bucket_cell::Bucket::list(&*cell.bucket, &prefix).await.unwrap();
        let held: u64 = listed.iter().map(|meta| meta.size).sum();
        let keys: Vec<&str> = listed.iter().map(|meta| meta.key.as_str()).collect();
        let bound = 2 * LIVE * BLOCK as u64;
        assert!(held < bound, "round {round}: {held} bytes held, of {bound} at most: {keys:?}");
    }
    // Each extent live reads as written, wherever the rounds put it.
    for &sequence in &live {
        assert_eq!(cell.read_in_bucket(sequence).await, noise(sequence), "{sequence}");
    }
}

/// cladefs: doc-27 (board 6214; the reviews of d33a6c360): a round whose relocation waits for
/// a guest's write in progress, holding the cell's commits back, holds back no upload: the
/// uploader, gathering what the guest commits after, waits for it in its own task, which goes
/// on once the write is done, the commit after it uploaded. Polled by the uploader's run, it
/// was never polled again once the run waited for the cell's commits in its gather.
#[tokio::test(flavor = "multi_thread")]
async fn rounds_waiting_for_writes_hold_no_upload_back() {
    let scenario = fail::FailScenario::setup();
    let interval = Duration::from_millis(20);
    let cell = Doc27::open(Options { interval, checkpoint_every: 1, ..Options::default() }).await;
    cell.segment(0..8, &[]).await;
    // The round after the next checkpoint waits until the write below is begun.
    fail::cfg("upload::relocating", "pause").unwrap();
    cell.segment(8..9, &[0, 1, 2, 3, 4, 5]).await;
    let write = cell.cell.begin_write(Doc27::extent(9), 0, BLOCK as u64).await.unwrap();
    write.write(&[9; BLOCK], 0).unwrap();
    fail::remove("upload::relocating");
    cell.until("the round waiting for the write", |cell| cell.cell.holding_writes()).await;
    // The interval runs out: the uploader gathers, closing the cell's open segment.
    tokio::time::sleep(10 * interval).await;
    write.finish();
    let committed = tokio::time::timeout(Duration::from_secs(10), cell.cell.commit(None));
    committed.await.expect("a commit held back").unwrap();
    let uploaded = cell.uploader.uploaded(cell.cell.last_commit().unwrap());
    tokio::time::timeout(Duration::from_secs(10), uploaded).await.expect("held back").unwrap();
    cell.until("relocated", |cell| [6, 7].iter().all(|&at| cell.in_bucket(at).is_none())).await;
    scenario.teardown();
}

/// TASK-539.9: a cell's disk under pressure urges its uploader, which closes the open segment
/// its guest is filling and uploads it at once, rather than wait out its interval, and
/// `filling_wait` past it, so that what it held leaves the disk (doc-22).
#[tokio::test(flavor = "multi_thread")]
async fn a_disk_under_pressure_has_its_cells_uploaded_at_once() {
    let host = Runtime {
        handle: tokio::runtime::Handle::current(),
        uploads: Budget::new(Limits::UNBOUNDED),
        keys: bucket_cell::keys::testing::host(),
    };
    let bucket =
        Arc::new(ObjectBucket::new(Arc::new(object_store::memory::InMemory::new()), "").unrouted());
    let dir = MemDir::new();
    let disk = dir.disk().disk().clone();
    let description = Description { block_size: 4096, capacity: 1 << 30, cell: CELL };
    LocalCell::make(&dir, description, Some("memory:///urged"), None).unwrap();
    lifecycle::create(&*bucket, CELL, 1).await.unwrap();
    // Its uploader uploads nothing it is not asked to within the hour.
    let hour = Duration::from_secs(3600);
    let options = Options { interval: hour, filling_wait: hour, ..Options::default() };
    let recovering = LocalCell::recover_unmade(dir).unwrap();
    let (cell, uploader) =
        open(recovering, crate::Options::default(), bucket.clone(), (1, options), &host)
            .await
            .unwrap();
    cell.made().unwrap();
    cell.write(ExtentId::new(CELL.id, 1), 0, &[1; 4096]).await.unwrap();
    cell.commit(None).await.unwrap();
    let epoch = cell.last_commit().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert!(uploader.last_uploaded() < epoch, "uploaded unasked");
    disk.urge();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while uploader.last_uploaded() < epoch {
        assert!(std::time::Instant::now() < deadline, "not uploaded as its disk urged it");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(cell.closed(usize::MAX).await.unwrap().len(), 0, "its segments left unuploaded");
}

/// TASK-539.12: the cell's disk refusing the record of an acknowledgement, as a full one does,
/// fails neither the cell nor its uploader: the record before it whole, the cell writes and
/// commits on, its segments wait in it, and the uploader records them once it can, which lets
/// them go. Before, the cell failed for good.
#[tokio::test(flavor = "multi_thread")]
async fn acknowledgements_a_full_disk_refuses_wait_for_room() {
    let scenario = fail::FailScenario::setup();
    let host = Runtime {
        handle: tokio::runtime::Handle::current(),
        uploads: Budget::new(Limits::UNBOUNDED),
        keys: bucket_cell::keys::testing::host(),
    };
    let bucket =
        Arc::new(ObjectBucket::new(Arc::new(object_store::memory::InMemory::new()), "").unrouted());
    let dir = MemDir::new();
    let description = Description { block_size: 4096, capacity: 1 << 30, cell: CELL };
    LocalCell::make(&dir, description, Some("memory:///full"), None).unwrap();
    lifecycle::create(&*bucket, CELL, 1).await.unwrap();
    let options = Options { interval: Duration::from_millis(20), ..Options::default() };
    let recovering = LocalCell::recover_unmade(dir).unwrap();
    let (cell, uploader) =
        open(recovering, crate::Options::default(), bucket.clone(), (1, options), &host)
            .await
            .unwrap();
    cell.made().unwrap();
    fail::cfg("wal::start", "return").unwrap();
    cell.write(ExtentId::new(CELL.id, 1), 0, &[1; 4096]).await.unwrap();
    cell.commit(None).await.unwrap();
    let epoch = cell.last_commit().unwrap();
    // Its interval's passes, 25 of them, each trying again.
    std::thread::sleep(Duration::from_millis(500));
    assert!(uploader.last_uploaded() < epoch, "counted uploaded, unrecorded");
    cell.write(ExtentId::new(CELL.id, 2), 0, &[2; 4096]).await.unwrap();
    cell.commit(None).await.unwrap();
    fail::remove("wal::start");
    uploader.uploaded(cell.last_commit().unwrap()).await.unwrap();
    assert_eq!(cell.closed(usize::MAX).await.unwrap().len(), 0, "its segments left in it");
    scenario.teardown();
}

/// board 5359: the cell's disk failing the record of an acknowledgement otherwise than a full
/// disk does (EIO) fails the uploader with it, and a wait on the upload returns that failure:
/// before, the record was tried again for ever, as a full disk's is, and the wait never ended.
#[tokio::test(flavor = "multi_thread")]
async fn acknowledgements_a_failing_disk_refuses_fail_the_uploader() {
    let scenario = fail::FailScenario::setup();
    let host = Runtime {
        handle: tokio::runtime::Handle::current(),
        uploads: Budget::new(Limits::UNBOUNDED),
        keys: bucket_cell::keys::testing::host(),
    };
    let bucket =
        Arc::new(ObjectBucket::new(Arc::new(object_store::memory::InMemory::new()), "").unrouted());
    let dir = MemDir::new();
    let description = Description { block_size: 4096, capacity: 1 << 30, cell: CELL };
    LocalCell::make(&dir, description, Some("memory:///failing"), None).unwrap();
    lifecycle::create(&*bucket, CELL, 1).await.unwrap();
    let options = Options { interval: Duration::from_millis(20), ..Options::default() };
    let recovering = LocalCell::recover_unmade(dir).unwrap();
    let (cell, uploader) =
        open(recovering, crate::Options::default(), bucket.clone(), (1, options), &host)
            .await
            .unwrap();
    cell.made().unwrap();
    fail::cfg("wal::start", "return(eio)").unwrap();
    cell.write(ExtentId::new(CELL.id, 1), 0, &[1; 4096]).await.unwrap();
    cell.commit(None).await.unwrap();
    let waited = uploader.uploaded(cell.last_commit().unwrap()).await;
    let failure = format!("{:#}", waited.unwrap_err());
    assert!(failure.contains("writing the WAL's start"), "{failure}");
    assert_eq!(uploader.failed().map(|failed| format!("{failed:#}")), Some(failure));
    fail::remove("wal::start");
    scenario.teardown();
}

/// board 8507: an uploader whose task panics fails what waits for it, naming the panic, and
/// stops, as one failing does: a commit waiting for its upload fails, its close returns, and
/// it counts among the process's running uploaders no more. Before, nothing set its failure,
/// nor said it stopped: the commit waited for ever, as did its close, and a drain counted it
/// running until its bound.
#[tokio::test(flavor = "multi_thread")]
async fn uploaders_that_panic_fail_their_waiters_and_stop() {
    let scenario = fail::FailScenario::setup();
    let host = Runtime {
        handle: tokio::runtime::Handle::current(),
        uploads: Budget::new(Limits::UNBOUNDED),
        keys: bucket_cell::keys::testing::host(),
    };
    let bucket =
        Arc::new(ObjectBucket::new(Arc::new(object_store::memory::InMemory::new()), "").unrouted());
    let dir = MemDir::new();
    let description = Description { block_size: 4096, capacity: 1 << 30, cell: CELL };
    LocalCell::make(&dir, description, Some("memory:///panicking"), None).unwrap();
    lifecycle::create(&*bucket, CELL, 1).await.unwrap();
    let options = Options { interval: Duration::from_millis(20), ..Options::default() };
    let recovering = LocalCell::recover_unmade(dir).unwrap();
    let (cell, uploader) =
        open(recovering, crate::Options::default(), bucket.clone(), (1, options), &host)
            .await
            .unwrap();
    cell.made().unwrap();
    assert_eq!(*uploaders().borrow(), 1);
    fail::cfg("upload::turn", "panic").unwrap();
    cell.write(ExtentId::new(CELL.id, 1), 0, &[1; 4096]).await.unwrap();
    cell.commit(None).await.unwrap();
    let waited = uploader.uploaded(cell.last_commit().unwrap()).await;
    fail::remove("upload::turn");
    let failure = format!("{:#}", waited.unwrap_err());
    assert_eq!(failure, "its uploader panicked: failpoint upload::turn panic");
    uploader.close().await;
    assert_eq!(*uploaders().borrow(), 0);
    scenario.teardown();
}

/// TASK-690: a cell's uploader swaps its cell's head no faster than a bucket of GCS's object
/// store takes it, however fast the cell commits: a guest waiting for each commit's upload,
/// then one closing segment after segment. Each swap names all uploaded since the last, the
/// uploads going on as it waits; the cell refuses no write of the head. doc-27 step 4 (board
/// 6220's AC #2): the head one object, swapped about once a second, as often as GCS takes it.
#[tokio::test(flavor = "multi_thread")]
async fn heads_are_swapped_as_fast_as_gcs_takes_them() {
    use bucket_cell::faults::{Fault, Faults, Faulty};
    use bucket_cell::object_bucket::Request;
    let host = Runtime {
        handle: tokio::runtime::Handle::current(),
        uploads: Budget::new(Limits::UNBOUNDED),
        keys: bucket_cell::keys::testing::host(),
    };
    let second = Duration::from_secs(1);
    let faults = Faults::new(Duration::ZERO, None);
    faults.set(Fault { rewrite_every: second, ..Fault::default() });
    let gcs = Faulty::new(Arc::new(object_store::memory::InMemory::new()), faults);
    let bucket = Arc::new(ObjectBucket::new(Arc::new(gcs), "").rewriting_every(second).unrouted());
    let dir = MemDir::new();
    let description = Description { block_size: 4096, capacity: 1 << 30, cell: CELL };
    LocalCell::make(&dir, description, Some("memory:///gcs"), None).unwrap();
    lifecycle::create(&*bucket, CELL, 1).await.unwrap();
    let recovering = LocalCell::recover_unmade(dir).unwrap();
    let (cell, uploader) =
        open(recovering, crate::Options::default(), bucket.clone(), (1, Options::default()), &host)
            .await
            .unwrap();
    cell.made().unwrap();
    // Its updates from now on: the head's swaps, the bucket's probe and the taking done.
    let updates = || bucket.stats().requests(Request::Update);
    let opened = updates();
    let started = std::time::Instant::now();
    let mut sequence = 0;
    let mut write = async |close: bool| {
        sequence += 1;
        cell.write(ExtentId::new(CELL.id, sequence), 0, &[sequence as u8; 4096]).await.unwrap();
        if close {
            cell.close_segment().unwrap();
        }
        cell.commit(None).await.unwrap();
        sequence
    };
    let mut waits = Vec::new();
    while started.elapsed() < 3 * second {
        write(false).await;
        let waiting = std::time::Instant::now();
        uploader.uploaded(cell.last_commit().unwrap()).await.unwrap();
        waits.push(waiting.elapsed());
    }
    for _ in 0..3 * IN_FLIGHT {
        write(true).await;
    }
    let last = write(false).await;
    uploader.uploaded(cell.last_commit().unwrap()).await.unwrap();
    let elapsed = started.elapsed();
    let stats = bucket.stats();
    assert_eq!(stats.failures(Request::Update), 0, "writes of the head refused");
    // Each swap a second after the last, and none of those seconds idle: the guest's commits
    // keep one waiting.
    let swaps = updates() - opened;
    let seconds = elapsed.as_secs();
    assert!(swaps <= seconds + 1, "{swaps} swaps of the head in {elapsed:?}");
    assert!(swaps + 1 >= seconds, "{swaps} swaps of the head in {elapsed:?}");
    // A commit waits for its swap's turn: up to a second, as this guest syncs faster than
    // one a second.
    let longest = waits.iter().max().unwrap();
    assert!(*longest < second + second / 2, "a commit waited {longest:?}: {waits:?}");
    drop(uploader);
    let state = bucket_cell::recover(&*bucket, CELL, None).await.unwrap().state;
    let sequences: Vec<u64> = state.extents.keys().map(|extent| extent.sequence).collect();
    assert_eq!(sequences, (1..=last).collect::<Vec<_>>());
}

/// TASK-79.19: a gather takes no more of the segments the cell closed than may upload at once,
/// with those uploading; the rest it leaves in the cell, until those it took are uploaded.
#[tokio::test(flavor = "multi_thread")]
async fn gathers_take_what_may_upload_at_once() {
    let host = Runtime {
        handle: tokio::runtime::Handle::current(),
        uploads: Budget::new(Limits::UNBOUNDED),
        keys: bucket_cell::keys::testing::host(),
    };
    let bucket =
        Arc::new(ObjectBucket::new(Arc::new(object_store::memory::InMemory::new()), "").unrouted());
    let dir = MemDir::new();
    let description = Description { block_size: 4096, capacity: 1 << 30, cell: CELL };
    LocalCell::make(&dir, description, Some("memory:///gathers"), None).unwrap();
    lifecycle::create(&*bucket, CELL, 1).await.unwrap();
    // Its uploader uploads nothing it is not asked to within the hour: the test gathers.
    let options = Options { interval: Duration::from_secs(3600), ..Options::default() };
    let recovering = LocalCell::recover_unmade(dir).unwrap();
    let (cell, uploader) =
        open(recovering, crate::Options::default(), bucket.clone(), (1, options), &host)
            .await
            .unwrap();
    cell.made().unwrap();
    // Each commit closes its segment.
    let closed = 2 * IN_FLIGHT + 1;
    for sequence in 1..=closed as u64 {
        cell.write(ExtentId::new(CELL.id, sequence), 0, &[sequence as u8; 4096]).await.unwrap();
        cell.close_segment().unwrap();
        cell.commit(None).await.unwrap();
    }
    let shared = &uploader.shared;
    let writer = shared.writer.lock().unwrap().clone().unwrap();
    let mut in_flight = InFlight::default();
    let mut taken = Vec::new();
    while shared.gather(&writer, &mut in_flight, (true, true)).await.unwrap().took {
        taken.push(in_flight.len());
        (async {
            while let Some(uploaded) = in_flight.next().await {
                uploaded.unwrap();
            }
            let through = writer.acknowledge().await.unwrap();
            shared.acknowledged(through, &mut in_flight).await.unwrap();
        })
        .await;
    }
    assert_eq!(taken, [IN_FLIGHT, IN_FLIGHT, 1]);
    assert_eq!(uploader.last_uploaded(), cell.last_commit().unwrap());
    drop(uploader);
    let state = bucket_cell::recover(&*bucket, CELL, None).await.unwrap().state;
    let sequences: Vec<u64> = state.extents.keys().map(|extent| extent.sequence).collect();
    assert_eq!(sequences, (1..=closed as u64).collect::<Vec<_>>());
}
