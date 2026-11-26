// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: background poller for the root bucket's global control-plane `events` object
//! (`bucket_cell::events`).
//!
//! Every extent daemon polls `events` via conditional GET (`If-None-Match` -> 304 Not Modified)
//! every [`DEFAULT_POLL`] (~20 s, overridable with `CLADEFSD_EVENTS_POLL_MS` for tests) and
//! applies global control-plane state without host fan-out:
//! - `shredded_tenants` -> [`crate::bucket::forget`] for each newly observed shredded tenant;
//! - `CellShredded` events -> [`crate::bucket::forget`] for the shredded cell;
//! - `decommissioned_hosts` -> logs an error if this daemon's ephemeral incarnation was
//!   decommissioned;
//! - `bucket_peers` -> updates [`super::peers::set`] when this bucket's peer generation advances.

use crate::Daemon;
use bucket_cell::events::{self, EventKind, Poller, Update};
use bucket_cell::keys::Scope;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

/// Default polling interval for the global `events` object (20 seconds).
pub(crate) const DEFAULT_POLL: Duration = Duration::from_secs(20);

/// Polling interval, overridden by `CLADEFSD_EVENTS_POLL_MS` when set (for tests / simulation).
fn poll_interval() -> Duration {
    if let Ok(raw) = node_local::env::var("CLADEFSD_EVENTS_POLL_MS")
        && let Ok(ms) = raw.parse::<u64>()
        && ms > 0
    {
        return Duration::from_millis(ms);
    }
    DEFAULT_POLL
}

/// Per-cell head GET timeout during post-gap reconciliation so an unavailable member bucket
/// cannot stall root `events` polling for healthy buckets (hotbed review 140 finding 519).
#[cfg(not(test))]
const RECONCILE_CELL_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(test)]
const RECONCILE_CELL_TIMEOUT: Duration = Duration::from_millis(150);

/// Maximum concurrent head GETs during post-gap reconciliation.
const RECONCILE_CONCURRENCY: usize = 16;

/// Starts the process default daemon's background `events` poller.
#[cfg(not(madsim))]
pub fn start() -> anyhow::Result<()> {
    start_on(Daemon::process())
}

/// Starts the daemon's background `events` poller as a supervised service.
pub(super) fn start_on(daemon: &Arc<Daemon>) -> anyhow::Result<()> {
    let bucket = match crate::storage::daemon_bucket_on(daemon) {
        Ok(bucket) => bucket,
        Err(err) => {
            log::warn!("cladefsd: events poller disabled, no daemon bucket: {err:?}");
            return Ok(());
        }
    };
    let interval = poll_interval();
    let polled = Arc::clone(daemon);
    crate::run::service(daemon, "cladefsd events", move || async move {
        run(polled, bucket, interval).await;
    })?;
    Ok(())
}

/// Polls `events` on the root authority bucket forever at `interval`.
async fn run(
    daemon: Arc<Daemon>,
    bucket: Arc<bucket_cell::object_bucket::RoutedBucket>,
    interval: Duration,
) {
    let mut poller = Poller::new();
    let mut state = Applied::default();
    let mut resolved: Option<(Arc<bucket_cell::object_bucket::ObjectBucket>, u32)> = None;

    loop {
        if resolved.is_none() {
            match (events::authority(&bucket).await, events::bucket_number(&bucket).await) {
                (Ok(auth), Ok(number)) => resolved = Some((auth, number)),
                (Err(err), _) | (_, Err(err)) => {
                    log::warn!("cladefsd: resolving events authority bucket: {err}");
                    tokio::time::sleep(interval).await;
                    continue;
                }
            }
        }
        if let Some((auth, my_bucket)) = &resolved {
            match poller.poll(&**auth).await {
                Ok(Some(update)) => state.apply(&daemon, &bucket, update, *my_bucket).await,
                Ok(None) if !state.reconcile_pending.is_empty() => {
                    state.reconcile(&daemon, &bucket).await;
                }
                Ok(None) => {}
                Err(err) => {
                    log::warn!("cladefsd: polling events: {err}");
                }
            }
        }
        tokio::time::sleep(interval).await;
    }
}

/// Local applied state tracked by the poller so each change is applied once.
#[derive(Default)]
struct Applied {
    shredded_tenants: BTreeSet<u64>,
    peers_generation: u64,
    decommissioned_logged: bool,
    seen_initial: bool,
    reconcile_pending: BTreeSet<cladefs_core::CellId>,
}

impl Applied {
    fn live_and_cached_cells(daemon: &Daemon) -> BTreeSet<cladefs_core::CellId> {
        let mut cells = BTreeSet::new();
        cells.extend(cladefs_core::halt::live_cells_on(&daemon.host));
        cells.extend(cladefs_fs::fuse::server::live_cells());
        cells.extend(daemon.host.bucket_state().keys().cells());
        cells.extend(daemon.host.cache().cached_cells());
        cells
    }

    async fn apply(
        &mut self,
        daemon: &Daemon,
        bucket: &bucket_cell::object_bucket::RoutedBucket,
        update: Update,
        my_bucket: u32,
    ) {
        let trimmed_before_start = !self.seen_initial && update.log.oldest_seq > 1;
        self.seen_initial = true;
        for tenant in update.log.shredded_set() {
            if self.shredded_tenants.insert(tenant) {
                let why = format!("tenant {tenant}: shredded (bucket events)");
                cladefs_core::bucket::forget_on(&daemon.host, Scope::Tenant(tenant), &why);
            }
        }
        for event in update.new_events {
            if let Some(EventKind::CellShredded(shredded)) = event.kind {
                let cell = shredded.cell_id();
                let why = bucket_cell::keys::shredded(cell).to_string();
                cladefs_core::bucket::forget_on(&daemon.host, Scope::Cell(cell), &why);
                self.reconcile_pending.remove(&cell);
            }
        }
        if !self.decommissioned_logged
            && let Ok(host) = cladefs_core::ephemeral_host::host_on(&daemon.host)
            && update.log.decommissioned_hosts.binary_search(&host.incarnation()).is_ok()
        {
            self.decommissioned_logged = true;
            log::error!(
                "cladefsd: this host's ephemeral incarnation {} has been decommissioned (bucket events)",
                cladefs_core::ephemeral_host::said(host.incarnation())
            );
        }
        if let Some(peers) = update.log.peers_for(my_bucket)
            && peers.generation > self.peers_generation
        {
            if cladefs_core::bucket::peers_on(&daemon.host).is_some() {
                match crate::peers::set_generation_on(
                    daemon,
                    peers.generation,
                    peers.addresses.clone(),
                ) {
                    Ok(true) => {
                        log::info!(
                            "cladefsd: updated peers for bucket {my_bucket} to generation {} ({} addresses)",
                            peers.generation,
                            peers.addresses.len()
                        );
                    }
                    Ok(false) => {}
                    Err(errno) => {
                        log::warn!(
                            "cladefsd: ignoring peers generation {} for bucket {my_bucket}: {errno:?}",
                            peers.generation
                        );
                    }
                }
            }
            self.peers_generation = peers.generation;
        }
        // cladefs: `CellShredded` events are only retained in the bounded `events` tail, so when
        // events were trimmed between polls (or before our first poll), queue every live or cached
        // cell for bounded-timeout reconciliation against its authoritative bucket head
        // (board 11665, hotbed review 140 finding 519).
        if update.skipped || trimmed_before_start {
            self.reconcile_pending.extend(Self::live_and_cached_cells(daemon));
        }
        if !self.reconcile_pending.is_empty() {
            self.reconcile(daemon, bucket).await;
        }
    }

    /// Reconciles pending live/cached cells on `daemon` against their bucket heads
    /// (`head.shredded`), forgetting any shredded cell whose `CellShredded` event was trimmed
    /// before this daemon saw it (board 11665). Uses bounded concurrency and a per-cell timeout so
    /// an unavailable member bucket cannot stall future root `events` polling; cells whose bucket
    /// failed or timed out remain queued for the next poll (hotbed review 140 finding 519).
    async fn reconcile(
        &mut self,
        daemon: &Daemon,
        bucket: &bucket_cell::object_bucket::RoutedBucket,
    ) {
        let live = Self::live_and_cached_cells(daemon);
        self.reconcile_pending.retain(|cell| live.contains(cell));
        if self.reconcile_pending.is_empty() {
            return;
        }
        let pending: Vec<cladefs_core::CellId> = self.reconcile_pending.iter().copied().collect();
        for chunk in pending.chunks(RECONCILE_CONCURRENCY) {
            let checks = chunk.iter().map(|&cell| async move {
                let res = tokio::time::timeout(
                    RECONCILE_CELL_TIMEOUT,
                    bucket_cell::head::get(bucket, cell),
                )
                .await;
                (cell, res)
            });
            for (cell, res) in futures::future::join_all(checks).await {
                match res {
                    Ok(Ok(Some(head))) if head.shredded => {
                        let why = bucket_cell::keys::shredded(cell).to_string();
                        cladefs_core::bucket::forget_on(&daemon.host, Scope::Cell(cell), &why);
                        self.reconcile_pending.remove(&cell);
                    }
                    Ok(Ok(_)) | Ok(Err(bucket_cell::Error::NoBucket(_))) => {
                        self.reconcile_pending.remove(&cell);
                    }
                    Ok(Err(err)) => {
                        log::warn!(
                            "cladefsd: reconciling shredded state of cell {cell} after events gap: {err}"
                        );
                    }
                    // Transient timeout on an unavailable member bucket: keep cell queued in
                    // `reconcile_pending` so the next poll retries without stalling healthy buckets.
                    Err(_elapsed) => {
                        log::warn!(
                            "cladefsd: timed out reconciling shredded state of cell {cell} after \
                             events gap; will retry on next poll"
                        );
                    }
                }
            }
        }
    }
}

#[cfg(all(test, not(madsim)))]
mod tests {
    use super::*;
    use bucket_cell::object_bucket::{ObjectBucket, RoutedBucket};
    use bucket_cell::routed::Resolve;
    use futures::future::BoxFuture;

    struct OutageResolve {
        member1: Arc<ObjectBucket>,
        member2: Arc<ObjectBucket>,
    }

    impl Resolve<ObjectBucket> for OutageResolve {
        fn open<'a>(
            &'a self,
            _home: &'a ObjectBucket,
            number: u16,
        ) -> BoxFuture<'a, Result<Arc<ObjectBucket>, bucket_cell::Error>> {
            let member1 = Arc::clone(&self.member1);
            let member2 = Arc::clone(&self.member2);
            Box::pin(async move {
                match number {
                    2 => Ok(member1),
                    3 => Ok(member2),
                    other => Err(bucket_cell::Error::NoBucket(other)),
                }
            })
        }
    }

    /// Hotbed review 140 finding 519: an unavailable member bucket during post-gap reconciliation
    /// must time out cleanly instead of stalling the root `events` poller, so healthy member
    /// cells still get reconciled and subsequent `CellShredded` events fence them promptly.
    #[test]
    fn unavailable_member_bucket_does_not_stall_reconciliation_or_healthy_shred() {
        let daemon = Daemon::new();
        daemon.block_on(async {
            let home = Arc::new(ObjectBucket::new(
                Arc::new(object_store::memory::InMemory::new()),
                "events-outage-home",
            ));
            let faults = bucket_cell::faults::Faults::new(Duration::ZERO, None);
            faults.set(bucket_cell::faults::Fault {
                failing: bucket_cell::faults::Failing::Unavailable,
                ..Default::default()
            });
            let member1 = Arc::new(ObjectBucket::new(
                Arc::new(bucket_cell::faults::Faulty::new(
                    Arc::new(object_store::memory::InMemory::new()),
                    faults,
                )),
                "events-outage-member1",
            ));
            let member2 = Arc::new(ObjectBucket::new(
                Arc::new(object_store::memory::InMemory::new()),
                "events-outage-member2",
            ));
            bucket_cell::identity::format(&*home, &bucket_cell::identity::Details::default())
                .await
                .unwrap();
            let routed = RoutedBucket::new(
                Arc::clone(&home),
                Arc::new(OutageResolve {
                    member1: Arc::clone(&member1),
                    member2: Arc::clone(&member2),
                }),
            );

            // `identity::format` assigns bucket 1 to `home`; use member buckets 2 and 3 so
            // `RoutedBucket` routes `cell1` through the unavailable member and `cell2` through the
            // healthy member rather than keeping `cell1` on `home`.
            let cell1 = cladefs_core::CellId { tenant: 0, id: (2u64 << 48) | 1 };
            let cell2 = cladefs_core::CellId { tenant: 0, id: (3u64 << 48) | 2 };
            let _halt1 = cladefs_core::halt::Halt::new_on(
                &daemon.host,
                std::path::Path::new("/tmp/c1"),
                cell1,
            );
            let halt2 = cladefs_core::halt::Halt::new_on(
                &daemon.host,
                std::path::Path::new("/tmp/c2"),
                cell2,
            );

            let mut state = Applied::default();
            let gap = Update {
                log: bucket_cell::events::EventLog::default(),
                new_events: Vec::new(),
                skipped: true,
            };
            tokio::time::timeout(Duration::from_secs(2), state.apply(&daemon, &routed, gap, 0))
                .await
                .expect("unavailable member bucket stalled events reconciliation");
            assert!(
                state.reconcile_pending.contains(&cell1),
                "unavailable member cell should stay pending for retry"
            );
            assert!(
                !state.reconcile_pending.contains(&cell2),
                "reachable member cell should have completed reconciliation"
            );

            // A subsequent `CellShredded` event for the healthy member must still fence `cell2`
            // even while member 1 remains unavailable.
            let mut poller = Poller::new();
            events::publish(&*home, events::Publish::CellShredded(cell2)).await.unwrap();
            let update = poller.poll(&*home).await.unwrap().expect("expected events update");
            tokio::time::timeout(Duration::from_secs(2), state.apply(&daemon, &routed, update, 0))
                .await
                .expect("unavailable member bucket stalled follow-up event processing");
            assert!(halt2.is_halted(), "healthy member cell should be fenced by CellShredded");
        });
    }
}
