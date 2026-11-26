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

/// Starts the process default daemon's background `events` poller.
#[cfg(all(target_os = "linux", not(madsim)))]
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
                Ok(Some(update)) => state.apply(&daemon, update, *my_bucket),
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
}

impl Applied {
    fn apply(&mut self, daemon: &Daemon, update: Update, my_bucket: u32) {
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
                match crate::peers::set_on(daemon, peers.addresses.clone()) {
                    Ok(()) => {
                        log::info!(
                            "cladefsd: updated peers for bucket {my_bucket} to generation {} ({} addresses)",
                            peers.generation,
                            peers.addresses.len()
                        );
                    }
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
    }
}
