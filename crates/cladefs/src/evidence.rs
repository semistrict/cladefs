// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: what a host does with the read evidence of the versions its cells are forks of
//! (TASK-702.2, TASK-702.3; `bucket_cell::evidence`, which says what evidence is).
//!
//! A cell opened for a guest, a fork of a version, has its version's evidence of its tenant's
//! ([`Evidence::opened`]). Its prediction, which the host reads once an interval ([`EVERY`]) for
//! all the forks of the version it opens, not for each, once it holds any recording, is fetched
//! into the host's cache as soon as it is read, as read-ahead: behind the guests' reads, from the
//! peers that own its parts first, many at once; and read from there into memory, as the guest's
//! page-ins would read it ([`prefetch`]): the host's cache is on disk, which the guest would
//! otherwise wait on a part at a time (board 6225). Its guest's boot is what the guest reads of the version in
//! its first [`RECORDED_FOR`]: every `READ`, `MAP` and fault of the version's extents the daemon
//! serves it ([`Evidence::read`]), as TASK-702.1 found a warm host's guest faults on none of what
//! its `MAP`s install. At the window's end, or as the cell closes or drops, if that comes first,
//! which a close waits for, the boot is measured against the prediction (`stats`), and, the first
//! fork the host opens of the version in an interval, written as a recording: by a task of its own
//! on the host's bucket runtime, never in the guest's way.

use crate::ExtentId;
use crate::bucket::HostFetcher;
use crate::open_cell::Arenas;
use crate::stats::{self, Counter};
use bucket_cell::evidence::{Of, Prediction, UNIT, Unit};
use bucket_cell::keys::Ring;
use bucket_cell::layout::CellId;
use bucket_cell::object_bucket::RoutedBucket;
use futures::future::{BoxFuture, Shared};
use futures::{FutureExt as _, StreamExt as _};
use local_cell::upload::Forked;
use local_cell::{LocalCell, Place};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long after a fork's cell opens what its guest reads is its boot: what a boot reads, as
/// TASK-702.1 measured it.
pub(crate) const RECORDED_FOR: Duration = Duration::from_secs(10);

/// How often a host records a fork of a version of a tenant's, and reads the tenant's evidence of
/// it again: at most once each an interval.
pub(crate) const EVERY: Duration = Duration::from_secs(600);

/// How many of a prediction's parts a fork fetches ahead at once, under the host's budget, which
/// bounds them all: TASK-702.1 modelled 16.5 s of a boot's waiting left at 32 at once, 7.4 s at
/// 64, none at 128.
const PREFETCHED_AT_ONCE: usize = 128;

/// A prediction, read once for the forks a host opens of a version in an interval: none if it
/// could not be read, which the next fork the host opens of the version reads again, as it does
/// one of no recordings.
pub(crate) type Predicted = Shared<BoxFuture<'static, Option<Arc<Prediction>>>>;

/// What the host keeps of a version's evidence of a tenant's.
#[derive(Default)]
struct Kept {
    /// When a fork here last began a recording of it.
    recorded: Option<Instant>,
    /// Its prediction, and when it was read.
    predicted: Option<(Instant, Predicted)>,
}

node_local::node_local! {
    /// What the host keeps of each version's evidence of each tenant's.
    static HOST: Mutex<HashMap<Of, Kept>> = Mutex::new(HashMap::new());
}

/// Forgets what the host keeps of every version's evidence, as a host restarted has: for tests
/// of what is read and recorded once an interval.
#[cfg(test)]
pub(crate) fn forget() {
    HOST.get().lock().unwrap().clear();
}

/// How many versions' evidence of a tenant's the host keeps: for tests of which cells read it.
#[cfg(test)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn kept() -> usize {
    HOST.get().lock().unwrap().len()
}

/// What a cell opened for a guest reads its extents through: what its prefetch reads them into
/// memory with, as its guest's reads would.
pub(crate) struct Reading {
    pub local: Arc<LocalCell<local_cell::HostDir>>,
    /// The arenas of physical memory its extents' pages are of.
    pub arenas: Arenas,
    /// What reads its extents' data from the bucket, and fetches it ahead.
    pub fetcher: Arc<HostFetcher>,
}

/// What a cell opened for a guest, a fork of a version, has of its version's evidence.
pub(crate) struct Evidence {
    #[cfg(test)]
    predicted: Predicted,
    boot: Arc<Boot>,
    /// What ends the boot, at the window's end or once told, by the sender's going.
    ending: Option<(tokio::sync::oneshot::Sender<()>, tokio::task::JoinHandle<()>)>,
    /// What fetches the prediction ahead of the guest, done, and what stops it as the cell goes.
    prefetching: (Shared<BoxFuture<'static, ()>>, futures::future::AbortHandle),
}

impl Evidence {
    /// The evidence of the cell of `own`, `forked`'s fork, its cell kept in `bucket`, read as
    /// `reading` says, opened now: its version's prediction, read now if the host read none of
    /// recordings in the interval, and read ahead; and its guest's boot, recorded if no fork here
    /// began a recording of the version in the interval.
    pub(crate) fn opened(
        forked: Forked,
        own: CellId,
        bucket: Arc<RoutedBucket>,
        reading: Reading,
    ) -> Self {
        let Forked { origin, ring } = forked;
        let of = Of { origin, tenant: own.tenant };
        let now = Instant::now();
        let fresh = |at: &Instant| now.saturating_duration_since(*at) < EVERY;
        let runtime = reading.fetcher.runtime().clone();
        let mut host = HOST.get().lock().unwrap();
        // What it read or recorded an interval ago or more it keeps no more, but for `of`.
        host.retain(|kept, Kept { recorded, predicted }| {
            *kept == of
                || recorded.as_ref().is_some_and(fresh)
                || predicted.as_ref().is_some_and(|(at, _)| fresh(at))
        });
        let kept = host.entry(of).or_default();
        // Shared while it is read, and once read of recordings; read again if it failed, or found
        // none, as a host that boots a version first does, whose first boot's recording lands
        // seconds later (board 5337).
        let of_recordings = |read: &Option<Arc<Prediction>>| {
            read.as_ref().is_some_and(|prediction| prediction.recordings > 0)
        };
        let read = |predicted: &Predicted| predicted.peek().is_none_or(of_recordings);
        let predicted = match &kept.predicted {
            Some((at, predicted)) if fresh(at) && read(predicted) => predicted.clone(),
            _ => {
                let predicted = predict(&runtime, bucket.clone(), of, ring.clone());
                kept.predicted = Some((now, predicted.clone()));
                predicted
            }
        };
        let recorded = match kept.recorded {
            Some(at) if fresh(&at) => false,
            _ => {
                kept.recorded = Some(now);
                true
            }
        };
        drop(host);
        let (prefetch, stop_prefetching) =
            futures::future::abortable(prefetch(predicted.clone(), reading));
        let prefetched = runtime.spawn(prefetch).map(drop).boxed().shared();
        let recorded = recorded.then_some(Recorded { bucket, ring });
        let boot = Arc::new(Boot::new(of, own.id, recorded));
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let (ended, predicting) = (boot.clone(), predicted.clone());
        let ending = runtime.spawn(async move {
            // The window's end, or the cell's close or drop, which lets `stop` go.
            drop(tokio::time::timeout(RECORDED_FOR, stopped).await);
            ended.end(predicting.peek().cloned().flatten().as_deref()).await;
        });
        let prefetching = (prefetched, stop_prefetching);
        let ending = Some((stop, ending));
        Self {
            #[cfg(test)]
            predicted,
            boot,
            ending,
            prefetching,
        }
    }

    /// The prediction of what the fork will read of its version.
    #[cfg(test)]
    pub(crate) fn predicted(&self) -> Predicted {
        self.predicted.clone()
    }

    /// Whether its boot is written as a recording of its version's.
    #[cfg(test)]
    pub(crate) fn recorded(&self) -> bool {
        self.boot.recorded.is_some()
    }

    /// Its prefetch, done: all it predicted read ahead, or stopped: for the daemon's tests.
    #[cfg(test)]
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn prefetched(&self) -> Shared<BoxFuture<'static, ()>> {
        self.prefetching.0.clone()
    }

    /// Whether it takes what the guest reads now into its boot: within its window.
    pub(crate) fn records(&self) -> bool {
        self.boot.within(Instant::now())
    }

    /// Takes the guest's read of `len` bytes at `offset` in `extent`, begun at `began`, into its
    /// boot: of the version's extents alone, within the window.
    pub(crate) fn read(&self, extent: ExtentId, offset: u64, len: u64, began: Instant) {
        self.boot.read(extent, offset, len, began);
    }

    /// Ends its boot, if it is not over yet, and waits for it to be measured and written: as its
    /// cell closes. Dropped, it has it ended behind it, waiting for nothing.
    pub(crate) async fn close(mut self) {
        if let Some((stop, ending)) = self.ending.take() {
            drop(stop);
            if let Err(err) = ending.await {
                log::warn!("cladefs: ending a boot: {err}");
            }
        }
    }
}

impl Drop for Evidence {
    fn drop(&mut self) {
        self.prefetching.1.abort();
    }
}

/// `of`'s prediction, read on `runtime` from `bucket`, at once, under `ring`: shared by the forks
/// of its version the host opens in the interval.
fn predict(
    runtime: &tokio::runtime::Handle,
    bucket: Arc<RoutedBucket>,
    of: Of,
    ring: Arc<Ring>,
) -> Predicted {
    let reading = runtime.spawn(async move {
        let read = bucket_cell::evidence::read(&*bucket, of, &ring.of(of.cell().id)).await;
        read.map(|read| Arc::new(Prediction::of(&read.recordings)))
    });
    // Unread, or its task stopped: no prediction, which only reads ahead; reads go on without it.
    reading.map(|read| read.ok().flatten()).boxed().shared()
}

/// Reads what `predicted` predicts the guest of a fork of its version reads ahead of it, once it
/// is read, as `reading` says, in its order, [`PREFETCHED_AT_ONCE`] units at once: of the extents
/// the fork's cell holds whose data is in the bucket, the parts fetched into the host's cache
/// behind the guests' reads, then read from there into memory ([`ahead`]). Nothing without a
/// host's cache, which would have it read into memory at the guest's priority.
async fn prefetch(predicted: Predicted, reading: Reading) {
    let Some(prediction) = predicted.await else { return };
    if !reading.fetcher.keeps_parts() {
        return;
    }
    let units = futures::stream::iter(prediction.units.iter().copied());
    let read = units.map(|unit| ahead(&reading, unit)).buffer_unordered(PREFETCHED_AT_ONCE);
    let [fetched, paged_in] = (read.fold([0, 0], |[all, ins], [fetched, paged_in]| {
        std::future::ready([all + fetched, ins + paged_in])
    }))
    .await;
    let predicted = prediction.units.len();
    log::debug!(
        "cladefs: of {predicted} units predicted, {fetched} parts fetched, {paged_in} read in"
    );
}

/// Reads `unit` ahead of the guest as `reading` says, if the fork's cell holds its extent sealed,
/// its data in the bucket: its parts fetched into the host's cache, then read into memory from
/// there, as a page-in of the guest's would. How many parts it fetched itself, and read in.
async fn ahead(reading: &Reading, unit: Unit) -> [u64; 2] {
    let Reading { local, arenas, fetcher } = reading;
    let Ok(Some(Place::Bucket(location))) = local.place(unit.extent) else { return [0, 0] };
    let Ok(Some((len, false))) = local.extent(unit.extent) else { return [0, 0] };
    let offset = unit.mib * UNIT;
    let fetched = match fetcher.fetch_ahead(unit.extent, &location, offset, UNIT).await {
        Ok(fetched) => fetched,
        Err(err) => {
            log::debug!("cladefs: fetching {:?} at {offset} ahead: {err}", unit.extent);
            return [0, 0];
        }
    };
    stats::add(Counter::Prefetches, fetched);
    let paged_in = match arenas.cached(local, fetcher.clone(), unit.extent, len).await {
        Ok(cached) => cached.read_ahead(offset, UNIT).await,
        Err(errno) => {
            log::debug!("cladefs: reading {:?} at {offset} ahead: {errno:?}", unit.extent);
            0
        }
    };
    stats::add(Counter::PrefetchPageIns, paged_in);
    [fetched, paged_in]
}

/// What writes a boot as a recording of its version's.
struct Recorded {
    bucket: Arc<RoutedBucket>,
    ring: Arc<Ring>,
}

/// What a fork's guest read of its version in its first [`RECORDED_FOR`]: measured against the
/// prediction once, and written, if it is recorded.
struct Boot {
    of: Of,
    /// The fork's own cell, whose extents are none of its version's.
    own: u64,
    opened: Instant,
    /// Each unit read, and when first, in milliseconds after the opening.
    units: Mutex<HashMap<(ExtentId, u64), u32>>,
    /// Where it is written, if it is.
    recorded: Option<Recorded>,
    /// Whether it is over, and takes no more reads.
    ended: AtomicBool,
}

impl Boot {
    fn new(of: Of, own: u64, recorded: Option<Recorded>) -> Self {
        let (opened, units, ended) = (Instant::now(), Mutex::default(), AtomicBool::new(false));
        Self { of, own, opened, units, recorded, ended }
    }

    /// Whether it takes what was read at `at`: within its window, and not over.
    fn within(&self, at: Instant) -> bool {
        !self.ended.load(Ordering::Relaxed)
            && at.saturating_duration_since(self.opened) <= RECORDED_FOR
    }

    /// Takes a read of `len` bytes at `offset` in `extent`, begun at `began`: each unit of the
    /// version's it covers, first read then, if not before, within the window, and
    /// [`bucket_cell::evidence::MOST_UNITS`].
    fn read(&self, extent: ExtentId, offset: u64, len: u64, began: Instant) {
        if extent.cell == self.own || len == 0 || !self.within(began) {
            return;
        }
        let at = began.saturating_duration_since(self.opened).as_millis();
        let at = u32::try_from(at).unwrap_or(u32::MAX);
        let mut units = self.units.lock().unwrap();
        for mib in offset / UNIT..=(offset + len - 1) / UNIT {
            let room = units.len() < bucket_cell::evidence::MOST_UNITS;
            match units.get_mut(&(extent, mib)) {
                Some(first) => *first = (*first).min(at),
                None if room => drop(units.insert((extent, mib), at)),
                None => {}
            }
        }
    }

    /// Ends it, by its ending task alone, letting go of its units: measured against `predicted`,
    /// the prediction its fork had then, if any; and written, if it is recorded and holds any
    /// unit, under the version's cell's newest DEK its fork holds.
    async fn end(&self, predicted: Option<&Prediction>) {
        self.ended.store(true, Ordering::Relaxed);
        let mut units: Vec<Unit> = (std::mem::take(&mut *self.units.lock().unwrap()).into_iter())
            .map(|((extent, mib), first)| Unit { extent, mib, first })
            .collect();
        measure(predicted, &units);
        let Some(Recorded { bucket, ring }) = &self.recorded else { return };
        if units.is_empty() {
            return;
        }
        units.sort_by_key(|unit| (unit.first, unit.extent, unit.mib));
        let cell = self.of.cell();
        let Some(encrypting) = ring.encrypting_as(cell.id) else {
            log::warn!("cladefs: writing the read evidence of {cell}: its fork holds no key of it");
            return;
        };
        match bucket_cell::evidence::write(&**bucket, self.of, &units, encrypting).await {
            Ok(key) => {
                log::debug!("cladefs: read evidence written at {key}: {} units", units.len())
            }
            Err(err) => log::warn!("cladefs: writing the read evidence of {cell}: {err}"),
        }
    }
}

/// Counts how well `predicted`, if a prediction was read, predicted `read`, a boot's units
/// ([`measured`]).
fn measure(predicted: Option<&Prediction>, read: &[Unit]) {
    let [hits, unread, unpredicted] = measured(predicted, read);
    stats::add(Counter::PredictedRead, hits);
    stats::add(Counter::PredictedUnread, unread);
    stats::add(Counter::ReadUnpredicted, unpredicted);
}

/// How well `predicted`, if a prediction was read, predicted `read`, a boot's units: how many of
/// them were predicted and read, predicted and unread, and read unpredicted (TASK-702.3).
fn measured(predicted: Option<&Prediction>, read: &[Unit]) -> [u64; 3] {
    let predicted: HashSet<(ExtentId, u64)> = (predicted.iter())
        .flat_map(|predicted| &predicted.units)
        .map(|unit| (unit.extent, unit.mib))
        .collect();
    let hits = read.iter().filter(|unit| predicted.contains(&(unit.extent, unit.mib))).count();
    [hits, predicted.len() - hits, read.len() - hits].map(|count| count as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bucket_cell::layout::{Origin, test_cell};

    /// A boot takes each unit of its version's a read covers, first read when it was first,
    /// within its window; none of its fork's own extents.
    #[test]
    fn boots_take_the_versions_units_first_read() {
        let of = Of { origin: Origin { parent: test_cell(3), at: 7, epoch: 1 }, tenant: 1 };
        let boot = Boot::new(of, 9, None);
        let opened = boot.opened;
        let extent = ExtentId { cell: 3, sequence: 1 };
        boot.read(extent, UNIT - 4096, 8192, opened + Duration::from_millis(5));
        boot.read(extent, 0, 4096, opened + Duration::from_millis(2));
        boot.read(ExtentId { cell: 9, sequence: 1 }, 0, 4096, opened);
        boot.read(extent, 3 * UNIT, 1, opened + RECORDED_FOR + Duration::from_millis(1));
        let units = boot.units.lock().unwrap().clone();
        assert_eq!(units, HashMap::from([((extent, 0), 2), ((extent, 1), 5)]));
    }

    /// A boot is measured by the units it read that were predicted, those predicted it did not
    /// read, and those it read unpredicted; with no prediction, all it read were unpredicted.
    #[test]
    fn boots_are_measured_against_their_predictions() {
        let extent = ExtentId { cell: 3, sequence: 1 };
        let unit = |mib| Unit { extent, mib, first: 0 };
        let prediction = Prediction { units: vec![unit(0), unit(1), unit(2)], recordings: 1 };
        let read = [unit(1), unit(2), unit(5), unit(6)];
        assert_eq!(measured(Some(&prediction), &read), [2, 1, 2]);
        assert_eq!(measured(None, &read), [0, 0, 4]);
    }
}
