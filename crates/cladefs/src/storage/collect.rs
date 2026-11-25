// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The collector of the file buckets images keep their cells in, each its own, beside its directory
//! (backlog doc-14, TASK-165.6): what reclaims what their cells no longer need there, the only
//! thing that deletes what images free (TASK-165.8). No `cladefs-gc` collects an image's own
//! bucket, as one does a host's: the processes that have images of it open do, a pass after an
//! image's commit, on a thread of its own, at most one every [`EVERY`]. A process has one collector
//! of a bucket, however many of its images it has open, an image and its forks, which reads again
//! only the cells' records that changed (`Collector`).

use bucket_cell::collector::{Collector, Report};
use bucket_cell::object_bucket::RoutedBucket;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often passes run.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Pace {
    /// How long at least goes from one pass's start to the next's.
    pub every: Duration,
}

/// A pass a minute.
const PACE: Pace = Pace { every: EVERY };

/// How long at least goes from one pass's start to the next's.
const EVERY: Duration = Duration::from_secs(60);

#[cfg(test)]
thread_local! {
    /// The pace of the collectors this thread makes, if not [`PACE`] ([`quicken`]).
    static QUICKENED: std::cell::Cell<Option<Pace>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
tokio::task_local! {
    /// The pace carried across `on_bucket_runtime` spawns (board 6543).
    static TASK_QUICKENED: Pace;
}

/// Has the collectors this thread makes from now on go at `pace`: as tests have passes run often,
/// and delete at once, among commits (TASK-291.3).
#[cfg(test)]
pub(crate) fn quicken(pace: Pace) {
    QUICKENED.with(|quickened| quickened.set(Some(pace)));
}

/// The test pace active on this task or thread, if any, to carry into `on_bucket_runtime`
/// (board 6543).
#[cfg(test)]
pub(crate) fn quickened() -> Option<Pace> {
    TASK_QUICKENED.try_with(|pace| *pace).ok().or_else(|| QUICKENED.with(std::cell::Cell::get))
}

/// Runs `work` with `pace` as its collector pace, if given (board 6543).
#[cfg(test)]
pub(crate) async fn with_quickened<T>(
    pace: Option<Pace>,
    work: impl std::future::Future<Output = T>,
) -> T {
    match pace {
        Some(pace) => TASK_QUICKENED.scope(pace, work).await,
        None => work.await,
    }
}

/// The pace of a collector made now.
fn pace() -> Pace {
    #[cfg(test)]
    if let Some(pace) = quickened() {
        return pace;
    }
    PACE
}

/// Per-host registry of image-bucket collectors, owned by [`crate::Host`].
pub(crate) struct Registry(Mutex<BTreeMap<String, Weak<Collecting>>>);

impl Registry {
    pub(crate) fn new() -> Self {
        Self(Mutex::new(BTreeMap::new()))
    }
}

/// The collector of `bucket` on `host`, at `url`, an image's own, which `host`'s images of it
/// share.
pub(crate) fn of_on(host: &crate::Host, url: &str, bucket: &Arc<RoutedBucket>) -> Arc<Collecting> {
    let mut collecting = host.collectors.0.lock().unwrap();
    collecting.retain(|_, collector| collector.strong_count() > 0);
    if let Some(collector) = collecting.get(url).and_then(Weak::upgrade) {
        return collector;
    }
    let pace = pace();
    let collector = Arc::new(Collecting {
        bucket: bucket.clone(),
        pace,
        state: Mutex::new(State { idle: Some(Collector::new()), ..Default::default() }),
    });
    collecting.insert(url.to_owned(), Arc::downgrade(&collector));
    collector
}

/// An image's own bucket's collector in this process. Dropped, it waits for the pass it runs, if
/// any.
pub(crate) struct Collecting {
    bucket: Arc<RoutedBucket>,
    pace: Pace,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The collector between passes, which keeps what it read from one to the next.
    idle: Option<Collector>,
    /// The pass running, which gives the collector back, and what it did.
    running: Option<JoinHandle<(Collector, Report)>>,
    /// When the last pass started.
    started: Option<Instant>,
    /// How many passes started.
    passes: u64,
    /// What the passes that ended did, together.
    collected: Report,
}

impl State {
    /// Takes the collector back from the pass that ran, if it ended: a pass's thread that
    /// panicked gives back none, and a new collector, which remembers nothing, takes
    /// its place.
    fn reap(&mut self) {
        let Some(running) = self.running.take_if(|running| running.is_finished()) else { return };
        self.ended(running);
    }

    /// Takes the collector back from `running`, waiting for it to end, and what it did.
    fn ended(&mut self, running: JoinHandle<(Collector, Report)>) {
        // A pass that panicked, its panic said: a new collector in its place, as reap says, said
        // here too (board 9414): a pass is work of an image's, which the next pass does again.
        let (collector, report) = running.join().unwrap_or_else(|_| {
            log::error!("cladefs: a pass of an image's collector panicked: a new one in its place");
            (Collector::new(), Report::default())
        });
        self.idle = Some(collector);
        self.collected += report;
    }
}

impl Collecting {
    /// Starts a pass, after an image's commit, if none runs and none started within its pace's
    /// [`Pace::every`].
    pub(crate) fn after_commit(&self) {
        let mut state = self.state.lock().unwrap();
        state.reap();
        let due = state.started.is_none_or(|started| started.elapsed() >= self.pace.every);
        let Some(mut collector) = state.idle.take().filter(|_| due) else { return };
        let bucket = self.bucket.clone();
        let pass = std::thread::Builder::new().name("cladefs collect".into()).spawn(move || {
            let report = pass(&mut collector, &bucket);
            (collector, report)
        });
        match pass {
            Ok(running) => {
                state.running = Some(running);
                state.started = Some(Instant::now());
                state.passes += 1;
            }
            Err(err) => {
                log::warn!("cladefs: starting a pass of an image's collector: {err}");
                state.idle = Some(Collector::new());
            }
        }
    }

    /// How many passes started.
    #[cfg(test)]
    pub(crate) fn passes(&self) -> u64 {
        self.state.lock().unwrap().passes
    }

    /// What the passes started did, together, the one running waited for.
    #[cfg(test)]
    pub(crate) fn collected(&self) -> Report {
        let mut state = self.state.lock().unwrap();
        if let Some(running) = state.running.take() {
            state.ended(running);
        }
        state.collected
    }

    /// Waits for the pass running, if any, then runs one: as tests look at what a pass reclaims.
    #[cfg(test)]
    pub(crate) fn pass_now(&self) -> Report {
        let mut state = self.state.lock().unwrap();
        if let Some(running) = state.running.take() {
            state.ended(running);
        }
        let mut collector = Collector::new();
        let collected =
            local_cell::upload::block_on(collector.collect(&*self.bucket, rand::random()));
        collected.unwrap().unwrap()
    }

    /// Runs passes as [`Collecting::pass_now`] does until two in a row delete nothing,
    /// every image of the bucket closed, and says what they left that they should have collected
    /// ([`bucket_cell::testing::uncollected`]): the space freed comes back (TASK-390). Two, as a
    /// pass over a dead cell retires the references that kept what its parents' cells reclaim at
    /// their next (TASK-395.4); and a pass retiring them again deletes nothing.
    #[cfg(test)]
    pub(crate) fn collected_alone(&self) -> Vec<String> {
        const PASSES: usize = 12;
        let mut quiet = 0;
        for _ in 0..PASSES {
            let report = self.pass_now();
            quiet = if report.deleted == 0 { quiet + 1 } else { 0 };
            if quiet == 2 {
                break;
            }
        }
        assert_eq!(quiet, 2, "{PASSES} passes alone, which kept doing something");
        let left = bucket_cell::testing::uncollected(&*self.bucket);
        local_cell::upload::block_on(left).unwrap()
    }
}

impl Drop for Collecting {
    fn drop(&mut self) {
        let running = self.state.get_mut().unwrap().running.take();
        if let Some(running) = running {
            // A pass that panicked: dropping, nothing to tell; its panic said itself.
            let _ = running.join();
        }
    }
}

/// A pass of `collector` over `bucket`, as a holder no other pass is: what it did, logged and
/// returned, or why it failed, logged. Nothing it could not read it deleted: the next pass tries
/// again.
fn pass(collector: &mut Collector, bucket: &Arc<RoutedBucket>) -> Report {
    // Where a test holds a pass up, to drop the images it collects for meanwhile (TASK-291.3).
    fail::fail_point!("collect::pass", |_| Report::default());
    // cladefs: copies of other buckets' cells' versions here, by its own cell (TASK-633.8).
    let own = bucket.own();
    let collected = collector.collect_with_own(&**bucket, Some(&*own), rand::random());
    let failed = match local_cell::upload::block_on(collected) {
        Ok(Ok(report)) => {
            let Report { deleted, retired, deleted_bytes, skipped, removed } = report;
            // Dead cells removed whole (TASK-455).
            log::info!(
                "cladefs: an image's bucket collected: deleted {deleted} ({deleted_bytes} bytes), \
                 retired {retired} references, {skipped} cells unchanged, {removed} dead cells \
                 removed"
            );
            return report;
        }
        Ok(Err(err)) => err.to_string(),
        Err(err) => err.to_string(),
    };
    log::warn!("cladefs: collecting an image's bucket: {failed}");
    Report::default()
}
