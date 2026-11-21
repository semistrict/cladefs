// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: what a host keeps of the read evidence of the versions its stores are forks of
//! (TASK-702.2; `bucket_cell::evidence`, which says what evidence is).
//!
//! A store opened for a guest, a fork of a version, has its version's evidence of its tenant's
//! ([`Evidence::opened`]): the prediction it makes, which the host reads once an interval
//! ([`EVERY`]) for all the forks of the version it opens, not for each; and, for the first fork it
//! opens of the version in an interval, a recording of what its guest reads of the version in its
//! first [`RECORDED_FOR`]: every `READ`, `MAP` and fault of the version's extents the daemon serves
//! it ([`Evidence::read`]), as TASK-702.1 found a warm host's guest faults on none of what its
//! `MAP`s install. The recording is written once, by a task of its own on the host's bucket runtime,
//! never in the guest's way: at the window's end, or as the store closes or drops, if that comes
//! first, which a close waits for.

use crate::ExtentId;
use bucket_cell::evidence::{Of, Prediction, UNIT, Unit};
use bucket_cell::keys::Ring;
use bucket_cell::layout::CellId;
use bucket_cell::object_bucket::ObjectBucket;
use futures::FutureExt as _;
use futures::future::{BoxFuture, Shared};
use local_cell::upload::Forked;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long after a fork's store opens what its guest reads is recorded: what a boot reads, as
/// TASK-702.1 measured it.
pub(crate) const RECORDED_FOR: Duration = Duration::from_secs(10);

/// How often a host records a fork of a version of a tenant's, and reads the tenant's evidence of
/// it again: at most once each an interval.
pub(crate) const EVERY: Duration = Duration::from_secs(600);

/// A prediction, read once for the forks a host opens of a version in an interval: none if it
/// could not be read, which the next fork the host opens of the version reads again.
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

/// What a store opened for a guest, a fork of a version, has of its version's evidence.
pub(crate) struct Evidence {
    #[cfg_attr(not(test), expect(dead_code, reason = "TASK-702.3 prefetches what it predicts"))]
    predicted: Predicted,
    recording: Option<Arc<Recording>>,
    /// What writes the recording, at the window's end or once told, by the sender's going.
    writing: Option<(tokio::sync::oneshot::Sender<()>, tokio::task::JoinHandle<()>)>,
}

impl Evidence {
    /// The evidence of the store of `own`, `forked`'s fork, its cell kept in `bucket`, opened now,
    /// whose tasks run on `runtime`: its version's prediction, read now if the host read it not in
    /// the interval; and a recording, if no fork here began one of the version in the interval.
    pub(crate) fn opened(
        forked: Forked,
        own: CellId,
        bucket: Arc<ObjectBucket>,
        runtime: &tokio::runtime::Handle,
    ) -> Self {
        let Forked { origin, ring } = forked;
        let of = Of { origin, tenant: own.tenant };
        let now = Instant::now();
        let fresh = |at: &Instant| now.saturating_duration_since(*at) < EVERY;
        let mut host = HOST.get().lock().unwrap();
        // What it read or recorded an interval ago or more it keeps no more, but for `of`.
        host.retain(|kept, Kept { recorded, predicted }| {
            *kept == of
                || recorded.as_ref().is_some_and(fresh)
                || predicted.as_ref().is_some_and(|(at, _)| fresh(at))
        });
        let kept = host.entry(of).or_default();
        let read = |predicted: &Predicted| predicted.peek().is_none_or(Option::is_some);
        let predicted = match &kept.predicted {
            Some((at, predicted)) if fresh(at) && read(predicted) => predicted.clone(),
            _ => {
                let predicted = predict(runtime, bucket.clone(), of, ring.clone());
                kept.predicted = Some((now, predicted.clone()));
                predicted
            }
        };
        let recording = match kept.recorded {
            Some(at) if fresh(&at) => None,
            _ => {
                kept.recorded = Some(now);
                Some(Arc::new(Recording::new(of, own.id, bucket, ring)))
            }
        };
        drop(host);
        let writing = recording.clone().map(|recording| {
            let (stop, stopped) = tokio::sync::oneshot::channel();
            let writing = runtime.spawn(async move {
                // The window's end, or the store's close or drop, which lets `stop` go.
                drop(tokio::time::timeout(RECORDED_FOR, stopped).await);
                recording.write().await;
            });
            (stop, writing)
        });
        Self { predicted, recording, writing }
    }

    /// The prediction of what the fork will read of its version.
    #[cfg_attr(not(test), expect(dead_code, reason = "TASK-702.3 prefetches what it predicts"))]
    pub(crate) fn predicted(&self) -> Predicted {
        self.predicted.clone()
    }

    /// Whether it records what the guest reads now: it has a recording, within its window.
    pub(crate) fn records(&self) -> bool {
        self.recording.as_ref().is_some_and(|recording| recording.within(Instant::now()))
    }

    /// Records the guest's read of `len` bytes at `offset` in `extent`, begun at `began`, if it
    /// records: of the version's extents alone, within the window.
    pub(crate) fn read(&self, extent: ExtentId, offset: u64, len: u64, began: Instant) {
        if let Some(recording) = &self.recording {
            recording.read(extent, offset, len, began);
        }
    }

    /// Has its recording written, if it has one not written yet, and waits for it: as its store
    /// closes. Dropped, it has it written behind it, waiting for nothing.
    pub(crate) async fn close(mut self) {
        if let Some((stop, writing)) = self.writing.take() {
            drop(stop);
            if let Err(err) = writing.await {
                log::warn!("cladefs: writing a read evidence: {err}");
            }
        }
    }
}

/// `of`'s prediction, read on `runtime` from `bucket`, at once, under `ring`: shared by the forks
/// of its version the host opens in the interval.
fn predict(
    runtime: &tokio::runtime::Handle,
    bucket: Arc<ObjectBucket>,
    of: Of,
    ring: Arc<Ring>,
) -> Predicted {
    let reading = runtime.spawn(async move {
        let read = bucket_cell::evidence::read(&*bucket, of, &ring.of(of.cell().id)).await;
        read.map(|read| Arc::new(Prediction::of(&read.recordings)))
    });
    reading.map(|read| read.ok().flatten()).boxed().shared()
}

/// What a fork's guest read of its version in its first [`RECORDED_FOR`], to be written once.
struct Recording {
    of: Of,
    /// The fork's own cell, whose extents are none of its version's.
    own: u64,
    opened: Instant,
    /// Each unit read, and when first, in milliseconds after the opening.
    units: Mutex<HashMap<(ExtentId, u64), u32>>,
    bucket: Arc<ObjectBucket>,
    ring: Arc<Ring>,
    /// Whether it is being written, and records no more.
    written: AtomicBool,
}

impl Recording {
    fn new(of: Of, own: u64, bucket: Arc<ObjectBucket>, ring: Arc<Ring>) -> Self {
        let (opened, units, written) = (Instant::now(), Mutex::default(), AtomicBool::new(false));
        Self { of, own, opened, units, bucket, ring, written }
    }

    /// Whether it records what was read at `at`: within its window, and not written yet.
    fn within(&self, at: Instant) -> bool {
        !self.written.load(Ordering::Relaxed)
            && at.saturating_duration_since(self.opened) <= RECORDED_FOR
    }

    /// Records a read of `len` bytes at `offset` in `extent`, begun at `began`: each unit of the
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

    /// Writes it, if it holds any unit, under the version's cell's newest DEK its fork holds: once,
    /// by its writing task alone, letting go of its units.
    async fn write(&self) {
        self.written.store(true, Ordering::Relaxed);
        let mut units: Vec<Unit> = (std::mem::take(&mut *self.units.lock().unwrap()).into_iter())
            .map(|((extent, mib), first)| Unit { extent, mib, first })
            .collect();
        if units.is_empty() {
            return;
        }
        units.sort_by_key(|unit| (unit.first, unit.extent, unit.mib));
        let cell = self.of.cell();
        let Some(encrypting) = self.ring.encrypting_as(cell.id) else {
            log::warn!("cladefs: writing the read evidence of {cell}: its fork holds no key of it");
            return;
        };
        let written = bucket_cell::evidence::write(&*self.bucket, self.of, &units, encrypting);
        match written.await {
            Ok(key) => {
                log::debug!("cladefs: read evidence written at {key}: {} units", units.len())
            }
            Err(err) => log::warn!("cladefs: writing the read evidence of {cell}: {err}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bucket_cell::layout::{Origin, test_cell};

    /// A recording takes each unit of its version's a read covers, first read when it was first,
    /// within its window; none of its fork's own extents.
    #[test]
    fn recordings_take_the_versions_units_first_read() {
        let objects = Arc::new(object_store::memory::InMemory::new());
        let bucket = Arc::new(ObjectBucket::new(objects, ""));
        let of = Of { origin: Origin { parent: test_cell(3), at: 7 }, tenant: 1 };
        let recording = Recording::new(of, 9, bucket, bucket_cell::testing::ring([test_cell(3)]));
        let opened = recording.opened;
        let extent = ExtentId { cell: 3, sequence: 1 };
        recording.read(extent, UNIT - 4096, 8192, opened + Duration::from_millis(5));
        recording.read(extent, 0, 4096, opened + Duration::from_millis(2));
        recording.read(ExtentId { cell: 9, sequence: 1 }, 0, 4096, opened);
        recording.read(extent, 3 * UNIT, 1, opened + RECORDED_FOR + Duration::from_millis(1));
        let units = recording.units.lock().unwrap().clone();
        assert_eq!(units, HashMap::from([((extent, 0), 2), ((extent, 1), 5)]));
    }
}
