// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the kept rounds (TASK-165.9's AC #4): a cell keeping its cell in a bucket, its
//! uploader uploading and acknowledging on a runtime's threads as the workload goes, crashes at a
//! random version; then the cell is taken from the bucket alone, its disk lost, and the cell is
//! opened again from what the crash left. Taken from the bucket, it holds a prefix of the commits
//! at least as late as the last its uploader acknowledged; opened from its disk, one at least as
//! late as the last made durable, its write-ahead log following the cell, which the cell
//! taking it fenced. Not a function of its seed alone, as the rounds of `rounds` are: the
//! uploader's threads interleave with the workload as they will.
//!
//! cladefs: passes of the collector run over the bucket all along the workload, compacting, each
//! after the last, on the host's runtime, as its collector would; one is cut at a random request
//! of its, as the host dies under it ([`Cut`]); and a full pass runs after the crash, before the
//! cell is taken and the cell opened again (TASK-291.2). Once it was, passes run alone, and
//! leave nothing they should have collected (TASK-390).

use super::host_runtime::HostRuntime;
use super::rounds::{BLOCK, CELL, Model, OPTIONS, Uploading, workload_uploading};
use bucket_cell::bucket::{ByteRange, Error as BucketError, Meta, Object, PutMode, Version};
use bucket_cell::collector;
use bucket_cell::fetch::{Budget, Limits};
use bucket_cell::object_bucket::{ObjectBucket, RoutedBucket};
use futures::FutureExt as _;
use local_cell::upload::{self, Runtime, Uploader};
use local_cell::{BucketData, Description, LocalCell, MemDir, Recovering};
use object_store::memory::InMemory;
use rand::rngs::StdRng;
use rand::{RngExt as _, SeedableRng as _};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
use std::time::Duration;

/// How many kept rounds the test runs.
pub const KEPT_ROUNDS: u64 = 100;

/// How many rounds of cut openings the test runs.
pub const CUT_ROUNDS: u64 = 30;

/// What the bucket's URL is to the cell's description; the rounds hand the uploader the bucket.
const URL: &str = "memory:///kept";

/// A host's: its cells' uploaders' runtime, and the bucket their cells are kept in, as it
/// reaches it: since its last crash, a copy of what the bucket held then ([`Host::crash`]).
struct Host {
    runtime: HostRuntime,
    bucket: RefCell<Arc<RoutedBucket>>,
    memory: RefCell<Arc<InMemory>>,
}

impl Host {
    fn new() -> Self {
        let runtime = HostRuntime::new();
        let memory = Arc::new(InMemory::new());
        let bucket = Arc::new(ObjectBucket::new(memory.clone(), "").unrouted());
        Self { runtime, bucket: RefCell::new(bucket), memory: RefCell::new(memory) }
    }

    /// The bucket, as the host reaches it now.
    fn bucket(&self) -> Arc<RoutedBucket> {
        self.bucket.borrow().clone()
    }

    /// The host crashes: what it had going reaches the bucket no more, as a dead host's work does
    /// not, but its writes landed so far. Its cells and uploaders, which go on until dropped,
    /// write to what it reached before; what opens on it from now on reaches a copy of that, as
    /// it is at one instant ([`InMemory::fork`], under its lock). Copied key by key, the uploader
    /// going on, a segment put after the listing and the head swapped before it was read left a
    /// copy whose head named a segment it lacked (TASK-432).
    fn crash(&self) {
        let copy = Arc::new(self.memory.borrow().fork());
        *self.bucket.borrow_mut() = Arc::new(ObjectBucket::new(copy.clone(), "").unrouted());
        *self.memory.borrow_mut() = copy;
    }

    /// The cell `recovering` recovered, opened by an uploader uploading every `interval`, as
    /// holder `holder` of its cell's head.
    async fn open(
        &self,
        recovering: Recovering<MemDir>,
        holder: u64,
        interval: Duration,
    ) -> Result<(Arc<LocalCell<MemDir>>, Uploader<MemDir>), upload::Failure> {
        let runtime = Runtime {
            handle: self.runtime.handle().clone(),
            uploads: Budget::new(Limits::UNBOUNDED),
            keys: bucket_cell::keys::testing::host(),
        };
        // Checkpoints often, so that the collector's passes find segments recoveries no longer
        // read, which they delete. cladefs: no relocation rounds (doc-27): their commits, which
        // change nothing a guest reads, take commit numbers the workload's model, by its own
        // commits' numbers, has no place for. The simulation's relocations profile crashes hosts
        // relocating, its oracle collapsing their commits (`sim`'s `applied`).
        let options = upload::Options {
            interval,
            checkpoint_every: 4,
            relocate: false,
            ..upload::Options::default()
        };
        upload::open(recovering, OPTIONS, self.bucket(), (holder, options), &runtime).await
    }

    /// What `cell` holds, its extents whose data is in the bucket read there.
    async fn contents(&self, cell: &LocalCell<MemDir>) -> Model {
        let mut extents = BTreeMap::new();
        for (extent, len) in cell.extents().unwrap() {
            let mut data = vec![0; len as usize];
            if let Err(err) = cell.read(extent, 0, &mut data) {
                let BucketData { location, .. } =
                    err.downcast_ref::<BucketData>().cloned().unwrap_or_else(|| panic!("{err:#}"));
                let bucket = self.bucket();
                let read = bucket_cell::testing::read(&bucket, extent, &location);
                data = read.await.unwrap();
            }
            extents.insert(extent, data);
        }
        Model { extents, root: cell.root().unwrap() }
    }
}

/// The host's bucket, as its collector reaches it: its first so many requests go, and none after,
/// which never complete, as a dead host's do not. A failing request would be no cut: a pass
/// retries what fails for ever.
struct Cut {
    bucket: Arc<RoutedBucket>,
    left: AtomicU64,
}

impl Cut {
    /// Whether the next request goes.
    fn goes(&self) -> bool {
        self.left.try_update(SeqCst, SeqCst, |left| left.checked_sub(1)).is_ok()
    }

    /// Whether it cut a request.
    fn cut(&self) -> bool {
        self.left.load(SeqCst) == 0
    }

    /// `request`, if it goes; one never completing, if not.
    fn request<T>(
        &self,
        request: impl Future<Output = T> + Send,
    ) -> impl Future<Output = T> + Send {
        let goes = self.goes();
        async move {
            if !goes {
                std::future::pending::<()>().await;
            }
            request.await
        }
    }
}

impl bucket_cell::Bucket for Cut {
    fn get(&self, key: &str) -> impl Future<Output = Result<Object, BucketError>> + Send {
        self.request(self.bucket.get(key))
    }

    fn get_range(
        &self,
        key: &str,
        range: ByteRange,
    ) -> impl Future<Output = Result<Vec<u8>, BucketError>> + Send {
        self.request(self.bucket.get_range(key, range))
    }

    fn head(&self, key: &str) -> impl Future<Output = Result<Meta, BucketError>> + Send {
        self.request(self.bucket.head(key))
    }

    fn list(&self, prefix: &str) -> impl Future<Output = Result<Vec<Meta>, BucketError>> + Send {
        self.request(self.bucket.list(prefix))
    }

    fn list_delimited(
        &self,
        prefix: &str,
        page: bucket_cell::bucket::Page<'_>,
    ) -> impl Future<Output = Result<bucket_cell::bucket::Listing, BucketError>> + Send {
        self.request(self.bucket.list_delimited(prefix, page))
    }

    fn put(
        &self,
        key: &str,
        bytes: bucket_cell::Bytes,
        mode: PutMode,
    ) -> impl Future<Output = Result<Version, BucketError>> + Send {
        self.request(self.bucket.put(key, bytes, mode))
    }

    fn delete(&self, key: &str) -> impl Future<Output = Result<(), BucketError>> + Send {
        self.request(self.bucket.delete(key))
    }

    fn now(&self) -> u64 {
        self.bucket.now()
    }

    fn pause(&self, failures: u32) -> impl Future<Output = ()> + Send {
        self.bucket.pause(failures)
    }
}

/// Passes of the collector over `cut`, one every `interval`, on a thread of their own, as the
/// host's collector's, for as long as the host lives: until the pass going is cut, or the host
/// crashes, which aborts them ([`Passes::abort`]).
struct Passes {
    /// What the passes done deleted.
    done: Arc<AtomicU64>,
    abort: futures::future::AbortHandle,
    thread: std::thread::JoinHandle<Result<Result<(), String>, futures::future::Aborted>>,
}

impl Passes {
    fn start(cut: Arc<Cut>, interval: Duration) -> Self {
        let (abort, registration) = futures::future::AbortHandle::new_pair();
        let done = Arc::<AtomicU64>::default();
        let doing = done.clone();
        // Made on its thread: the passes' future, which the thread alone polls, need not be Send.
        let thread = std::thread::spawn(move || {
            let passes = async move {
                for holder in 100.. {
                    let pass = collector::collect(&*cut, holder).await;
                    let report = pass.map_err(|err| format!("pass {holder}: {err}"))?;
                    doing.fetch_add(report.deleted as u64, SeqCst);
                    // The thread's alone: the passes wait for nothing else on it.
                    std::thread::sleep(interval);
                }
                unreachable!("passes for ever")
            };
            futures::executor::block_on(futures::future::Abortable::new(passes, registration))
        });
        Self { done, abort, thread }
    }

    /// The host dies with its collector: what its pass going did so far stays in the bucket.
    /// Returns how many objects the passes done deleted; or how a pass failed.
    fn abort(self) -> Result<u64, String> {
        self.abort.abort();
        if let Ok(failed) = self.thread.join().unwrap() {
            failed?;
        }
        Ok(self.done.load(SeqCst))
    }
}

/// What a round's first crash left: the directory, what each commit began with, the last made
/// durable, the last the uploader acknowledged, and the first of the workload's.
struct Crashed {
    dir: MemDir,
    epochs: Vec<Model>,
    durable: usize,
    acknowledged: usize,
    first: usize,
    /// Whether the host's collector was cut in a pass before the crash.
    cut: bool,
    /// How many objects its passes deleted before.
    collected: u64,
}

const DESCRIPTION: Description =
    Description { block_size: BLOCK as u32, capacity: 4 << 20, cell: CELL };

/// A cell made on `host`, kept in its bucket, its uploader uploading every `interval`, crashing
/// in a random workload, as `rng` says, as its file system fails from a random version on.
async fn crashed(host: &Host, round: u64, rng: &mut StdRng, interval: Duration) -> Crashed {
    let dir = MemDir::new();
    LocalCell::make(&dir, DESCRIPTION, Some(URL), None).unwrap();
    let bucket = host.bucket();
    bucket_cell::lifecycle::create(&*bucket, CELL, 1).await.unwrap();
    let (cell, uploader) =
        host.open(LocalCell::recover_unmade(dir.clone()).unwrap(), 1, interval).await.unwrap();
    cell.made().unwrap();
    // The commits opening it made, the cell's capacity set, hold nothing.
    let first = cell.last_commit().unwrap() as usize;
    let mut epochs = vec![Model::default(); first + 1];
    // cladefs: a pass's read of its cell's head, a GET, one more.
    let most = 401;
    let cut = Arc::new(Cut { bucket: host.bucket(), left: rng.random_range(0..most).into() });
    let passes = Passes::start(cut.clone(), interval);
    dir.fail_after(Some(rng.random_range(0..600)));
    // At a random step among its first: a burst, its commit spanning segments, and the uploader
    // asked to upload it, waited for until it is acknowledged (board 1738, board 1852).
    let at = rng.random_range(0..20);
    let acknowledge = |epoch| {
        let uploaded = uploader.uploaded(epoch);
        async move { Ok(uploaded.await?) }.boxed()
    };
    let uploading: Uploading<'_> = (at, &acknowledge);
    let (durable, _) =
        workload_uploading(rng, &cell, &mut epochs, (Model::default(), first), Some(uploading))
            .await;
    let acknowledged = uploader.last_uploaded() as usize;
    let dir = dir.crash(round);
    let collected = passes.abort().unwrap_or_else(|err| panic!("round {round}: {err}"));
    host.crash();
    // The host is gone: its uploader, which stops at the cell's failure, or uploads what it
    // has left where none sees it, and its cell.
    drop(uploader);
    drop(cell);
    Crashed { dir, epochs, durable, acknowledged, first, cut: cut.cut(), collected }
}

/// Opens the cell `crashed` left on `host`, as holder `holder`, and checks it holds a prefix of
/// its commits at least as late as `least`: returns that commit's epoch.
async fn reopened(
    host: &Host,
    round: u64,
    crashed: &Crashed,
    dir: MemDir,
    holder: u64,
    least: usize,
) -> usize {
    let recovering = LocalCell::recover(dir)
        .unwrap_or_else(|err| panic!("round {round}: the cell doesn't recover: {err:#}"));
    let (cell, uploader) = host
        .open(recovering, holder, HOUR)
        .await
        .unwrap_or_else(|err| panic!("round {round}: the cell doesn't open: {err}"));
    let epochs = &crashed.epochs;
    let epoch = cell.last_commit().unwrap() as usize;
    let Crashed { durable, acknowledged, .. } = crashed;
    assert!(
        (least..epochs.len()).contains(&epoch),
        "round {round}: opened at epoch {epoch}, of {least}..{}, {durable} durable, \
         {acknowledged} acknowledged",
        epochs.len()
    );
    assert_eq!(host.contents(&cell).await, epochs[epoch], "round {round}: at epoch {epoch}");
    drop(uploader);
    drop(cell);
    epoch
}

/// An uploader's interval that uploads nothing a test does not wait for.
const HOUR: Duration = Duration::from_secs(3600);

/// What a round of the kept rounds found: whether its uploader had acknowledged a commit of the
/// workload's when it crashed, whether the cell opened from its disk held commits the bucket
/// lacked, whether its collector was cut in a pass, and whether its passes deleted objects before
/// the crash.
pub struct Found {
    pub acknowledged: bool,
    pub ahead: bool,
    pub cut: bool,
    pub deleted: bool,
}

/// Round `round` of the kept rounds (see the module's docs).
pub async fn kept(round: u64) -> Found {
    let host = Host::new();
    let mut rng = StdRng::seed_from_u64(round);
    let interval = Duration::from_millis(rng.random_range(1..=20));
    let crashed = crashed(&host, round, &mut rng, interval).await;
    let Crashed { epochs, acknowledged, durable, first, cut, collected, .. } = &crashed;

    // A pass after the crash, over what it left, completes, as the next host's collector's would.
    let bucket = host.bucket();
    let pass = collector::collect(&*bucket, 1 << 20);
    pass.await.unwrap_or_else(|err| panic!("round {round}: the pass: {err}"));

    // Its disk lost, another host takes the cell from the bucket.
    let elsewhere = MemDir::new();
    LocalCell::make(&elsewhere, DESCRIPTION, Some(URL), None).unwrap();
    let recovering = LocalCell::recover_unmade(elsewhere).unwrap();
    let (taken, uploader) = host.open(recovering, 2, interval).await.unwrap();
    taken.made().unwrap();
    let epoch = taken.last_commit().unwrap() as usize;
    assert!(
        (*acknowledged..epochs.len()).contains(&epoch),
        "round {round}: taken at epoch {epoch}, of {acknowledged}..{}",
        epochs.len()
    );
    assert_eq!(host.contents(&taken).await, epochs[epoch], "round {round}: taken at epoch {epoch}");
    drop(uploader);
    drop(taken);

    // Its disk kept, the cell opens again: its log follows the cell, which the taking fenced
    // past no commit, and what it made durable it holds.
    let reopened =
        reopened(&host, round, &crashed, crashed.dir.clone(), 3, (*durable).max(epoch)).await;
    collected_alone(&host, round).await;
    Found {
        acknowledged: acknowledged > first,
        ahead: reopened > epoch,
        cut: *cut,
        deleted: *collected > 0,
    }
}

/// How many passes, alone, [`collected_alone`] lets run before one does nothing.
const PASSES_ALONE: u64 = 8;

/// Runs passes over `host`'s bucket, alone, every cell gone, until one does nothing, and checks
/// they left nothing they should have collected: the space the round's commits freed comes back,
/// not only what they hold stays (TASK-390).
async fn collected_alone(host: &Host, round: u64) {
    let bucket = host.bucket();
    let mut quiet = false;
    for pass in 0..PASSES_ALONE {
        let pass = collector::collect(&*bucket, (2 << 20) + pass);
        let report = pass.await.unwrap_or_else(|err| panic!("round {round}: {err}"));
        if report == collector::Report::default() {
            quiet = true;
            break;
        }
    }
    assert!(quiet, "round {round}: {PASSES_ALONE} passes alone each did something");
    let left = bucket_cell::testing::uncollected(&*bucket);
    assert_eq!(left.await, Vec::<String>::new(), "round {round}");
}

/// Round `round` of the cut openings: the cell a kept round's first crash left opened again, its
/// file system failing at each of the opening's writes in turn, the host crashing there; opened
/// again after, it holds a prefix at least as late as its last durable commit, whatever the cut
/// opening wrote: a segment closed again from its log, the WAL's start moved on past what the
/// bucket acknowledged, the files that left for the bucket, a capacity restated. Returns how many
/// writes the opening made.
pub async fn cut(round: u64) -> u64 {
    let host = Host::new();
    let mut rng = StdRng::seed_from_u64(round);
    let interval = Duration::from_millis(rng.random_range(1..=20));
    let crashed = crashed(&host, round, &mut rng, interval).await;
    let mut holder = 10;
    for writes in 0.. {
        // A copy of what the crash left, which the opening cut at its write `writes` changes.
        let dir = crashed.dir.crash(0);
        dir.fail_after(Some(writes));
        let opening = LocalCell::recover(dir.clone()).map_err(upload::Failure::from);
        let opened = match opening {
            Ok(recovering) => host.open(recovering, holder, HOUR).await,
            Err(err) => Err(err),
        };
        let whole = opened.is_ok() && !dir.has_failed();
        drop(opened);
        if whole {
            return writes;
        }
        let epoch =
            reopened(&host, round, &crashed, dir.crash(writes), holder + 1, crashed.durable).await;
        assert!(epoch >= crashed.acknowledged, "round {round}: cut at {writes}");
        holder += 2;
    }
    unreachable!()
}
