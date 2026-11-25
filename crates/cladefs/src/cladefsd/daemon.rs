// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: explicit per-daemon context (`Arc<Daemon>`), owning every subsystem of one extent
//! daemon and its consolidated Tokio runtime (Wave 4C.5, board 10774).
//!
//! Replacing `node_local!` statics with `Arc<Daemon>` lets multiple daemons coexist cleanly in one
//! process (tests and `madsim` simulations) without per-node static indirection or manual reset
//! hooks, and consolidates the daemon's four async service runtimes (`IO`, `control`, `peers`,
//! `memory governor`) into one shared multi-threaded runtime (`cladefsd`).
//!
//! **Latency isolation:** `crate::cache` keeps its dedicated `cladefs pager` runtime (`PAGER_THREADS`
//! workers) separate from `Daemon::runtime` because page-ins run synchronous decompression, disk
//! reads, and `slab.write` directly on pager worker threads and size their 1 MiB scratch buffer
//! pool to `PAGER_THREADS` (`process::PAGE_INS`). Merging them would either let control/peer/metric
//! RPCs delay latency-critical DAX page faults or let synchronous decompression starve control and
//! VM socket I/O.

#[cfg(not(madsim))]
use super::control;
use super::{cells, disk, drain, governor, memory, migration, peers, pool, process, server, stats};
use bucket_cell::cache::Cache;
use bucket_cell::object_bucket::RoutedBucket;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once, OnceLock, Weak};
use tokio::sync::{Semaphore, watch};

/// Worker threads for the daemon's consolidated async runtime (`cladefsd`), replacing the separate
/// `IO` (2), `control` (2), `peers` (2), and `memory governor` (1) runtimes.
#[cfg(not(madsim))]
const RUNTIME_THREADS: usize = 4;

/// Explicit state and runtime of one extent daemon instance.
pub struct Daemon {
    /// Explicit per-host context owning `local_cell::Host`, `bucket_cell::Host`, slabs, extent
    /// cache, bucket/KMS state, ephemeral host, halts, evidence, and collectors (board 10962).
    pub(crate) host: Arc<crate::Host>,
    /// Whether the daemon accepts VM processes' connections (`serve`), reported by gRPC health.
    pub(super) serving: watch::Sender<bool>,
    /// Whether the control API's health service has reported the latest `serving` state.
    pub(super) reported: watch::Sender<bool>,
    /// Whether this daemon serves the control API.
    pub(super) control_serving: AtomicBool,
    /// Whether the daemon is draining (`drain`).
    pub(super) draining: watch::Sender<bool>,
    /// Active VM connections counted for drain.
    pub(super) connections_work: drain::Work,
    /// Active control API calls counted for drain.
    pub(super) calls_work: drain::Work,
    /// Consolidated Tokio runtime for VM socket I/O, control gRPC, peer gRPC, and memory governor.
    /// Under `madsim`, tasks spawn directly onto the simulated node's runtime.
    #[cfg(not(madsim))]
    runtime: OnceLock<tokio::runtime::Runtime>,
    /// Open cell registry and holds (`CellTable`).
    pub(super) table: cells::Table,
    /// The daemon's bucket handle, initialized on first use or injected in tests.
    pub(super) bucket: OnceLock<Arc<RoutedBucket>>,
    /// The daemon's part cache, opened once from `CLADEFSD_PART_CACHE`.
    pub(super) part_cache: Mutex<Option<Option<Arc<Cache>>>>,
    /// Listening VM socket FDs shut down on drain.
    pub(super) accepting: Mutex<Vec<std::os::fd::RawFd>>,
    /// Total active VM connections across all tenants.
    pub(super) connected: Mutex<usize>,
    /// Default tenant, created on first VM connection to the daemon's own socket.
    pub(super) default_tenant: OnceLock<Arc<server::Tenant>>,
    /// Resolved `CLADEFSD_ROOTS` directories, if configured.
    pub(super) roots: OnceLock<Result<Option<Arc<[PathBuf]>>, String>>,
    /// Concurrency semaphore for background cell flushes (`server::FLUSHES`).
    pub(super) flush_turns: Semaphore,
    /// Connected VM processes tracked for quarantine and watchdog.
    pub(super) vm_peers: Mutex<Vec<Arc<server::Peer>>>,
    /// Starts the VM peer watchdog and registers quarantine once per daemon.
    pub(super) watching_vm_peers: Once,
    /// Count of wedged VM processes (killed but not yet exited past `KILLED_EXITING`).
    pub(super) wedged: AtomicU64,
    /// Thread pool serving blocking VM requests.
    pool: OnceLock<pool::Pool>,
    /// Read trace output directory (`CLADEFSD_READ_TRACES`).
    pub(super) read_traces: OnceLock<Option<PathBuf>>,
    /// Per-tenant memory limits, weights, held parts, and in-flight budgets.
    pub(super) memory: memory::State,
    /// Memory governor state (`full`, `pressured`, `fulls`, `floor`).
    pub(super) governor: governor::State,
    /// Per-tenant disk limits, weights, and disk governor startup state.
    pub(super) disk: disk::State,
    /// Mutual TLS credentials for peer gRPC (`CLADEFSD_PEER_TLS`).
    pub(super) peer_credentials: Mutex<Option<peers::Credentials>>,
    /// Outgoing and incoming ephemeral cell migration state.
    pub(super) migrations: migration::State,
    /// Active VM connection reports for `GetStats` and `/metrics`.
    pub(super) stats_reports: Mutex<Vec<Weak<stats::Report>>>,
    /// Process memory budget and RSS/CPU sample state.
    pub(super) process: Mutex<process::State>,
    /// Pages dirtied writing the periodic stats file (`CLADEFSD_STATS`).
    pub(super) stats_written: AtomicU64,
    /// FUSE mount state (outside `madsim`).
    #[cfg(not(madsim))]
    pub(super) mount: super::mount::State,
    /// Number of times `kept_or_open` opened a version/kept cell store (TASK-549, board 2494).
    #[cfg(test)]
    pub(super) kept_opens: AtomicU64,
}

impl Daemon {
    /// Creates a fresh extent daemon instance with an isolated [`crate::Host`], empty registries,
    /// and its own consolidated runtime (created lazily on first async use outside `madsim`).
    pub fn new() -> Arc<Self> {
        Self::with_host(crate::Host::new())
    }

    /// Creates a fresh extent daemon instance backed by `host`.
    pub fn with_host(host: Arc<crate::Host>) -> Arc<Self> {
        let daemon = Arc::new(Self {
            host,
            serving: watch::channel(false).0,
            reported: watch::channel(false).0,
            control_serving: AtomicBool::new(false),
            draining: watch::channel(false).0,
            connections_work: drain::Work::new(),
            calls_work: drain::Work::new(),
            #[cfg(not(madsim))]
            runtime: OnceLock::new(),
            table: cells::Table::new(),
            bucket: OnceLock::new(),
            part_cache: Mutex::new(None),
            accepting: Mutex::new(Vec::new()),
            connected: Mutex::new(0),
            default_tenant: OnceLock::new(),
            roots: OnceLock::new(),
            flush_turns: Semaphore::new(server::FLUSHES),
            vm_peers: Mutex::new(Vec::new()),
            watching_vm_peers: Once::new(),
            wedged: AtomicU64::new(0),
            pool: OnceLock::new(),
            read_traces: OnceLock::new(),
            memory: memory::State::new(),
            governor: governor::State::default(),
            disk: disk::State::new(),
            peer_credentials: Mutex::new(None),
            migrations: migration::State::new(),
            stats_reports: Mutex::new(Vec::new()),
            process: process::State::new(),
            stats_written: AtomicU64::new(0),
            #[cfg(not(madsim))]
            mount: super::mount::State::default(),
            #[cfg(test)]
            kept_opens: AtomicU64::new(0),
        });
        server::register_daemon(&daemon);
        daemon
    }

    /// The per-host context backing this daemon.
    pub fn host(&self) -> &Arc<crate::Host> {
        &self.host
    }

    /// The process's default daemon instance, for standalone tests and legacy free-function entry
    /// points outside `madsim`. Shares [`crate::Host::process`] so crate-level free functions and
    /// daemon entry points act on the same host state. Under `madsim`, callers always pass an
    /// explicit `Arc<Daemon>`.
    #[cfg(not(madsim))]
    pub fn process() -> &'static Arc<Self> {
        // Outside the simulation: single default daemon for non-madsim tests/examples.
        static PROCESS: OnceLock<Arc<Daemon>> = OnceLock::new();
        PROCESS.get_or_init(|| Self::with_host(crate::Host::process().clone()))
    }

    /// The daemon's consolidated Tokio runtime (outside `madsim`), shared by VM socket I/O,
    /// control gRPC, peer gRPC, and the memory governor.
    #[cfg(not(madsim))]
    pub(super) fn runtime(&self) -> &tokio::runtime::Runtime {
        self.runtime.get_or_init(|| {
            // cladefs: the daemon's seam: one consolidated runtime outside the simulation.
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(RUNTIME_THREADS)
                .max_blocking_threads(control::BLOCKING_THREADS)
                .thread_name("cladefsd")
                .enable_all()
                .build()
                .expect("cladefsd's runtime")
        })
    }

    /// Runs `future` to completion on this daemon's consolidated runtime (outside `madsim`) or via
    /// `local_cell::upload::wait_on` (under `madsim`, where binary targets still compile).
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        #[cfg(not(madsim))]
        {
            // cladefs: outside the simulation: outermost entry point (`cladefsd` binary / tests).
            self.runtime().block_on(future)
        }
        #[cfg(madsim)]
        {
            local_cell::upload::wait_on_host(self.host.local(), future)
        }
    }

    /// Spawns `future` on this daemon's runtime (outside `madsim`) or on the current `madsim`
    /// node's runtime (under `madsim`).
    pub(super) fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        #[cfg(not(madsim))]
        return self.runtime().spawn(future);
        #[cfg(madsim)]
        return tokio::spawn(future);
    }

    /// The thread pool serving blocking VM requests for this daemon.
    pub(super) fn pool(&self) -> &pool::Pool {
        self.pool.get_or_init(pool::Pool::requests)
    }

    /// Whether this daemon serves VM processes: booted, and not draining.
    pub fn serving(&self) -> bool {
        *self.serving.borrow()
    }

    /// Says this daemon stops serving, waiting for its health service (if running) to report
    /// `NOT_SERVING`.
    pub async fn stop_serving(&self) {
        self.serving.send_replace(false);
        if self.control_serving.load(Ordering::Acquire) {
            let mut reported = self.reported.subscribe();
            // The sender lives as long as `Daemon`: ends once the reporter reports.
            let _ = reported.wait_for(|serving| !*serving).await;
        }
    }

    /// Boots this daemon serving VM processes at `socket`, with `memory` bytes fixed or governed.
    #[cfg(any(target_os = "linux", madsim))]
    pub async fn boot(self: &Arc<Self>, socket: &Path, memory: Option<u64>) -> anyhow::Result<()> {
        super::boot::boot_daemon(self, socket, memory).await
    }

    /// Drains this daemon within `within`.
    pub async fn drain(self: &Arc<Self>, within: std::time::Duration) -> Result<(), drain::Left> {
        drain::drain_daemon(self, within).await
    }

    /// Serves VM processes connecting to `listener` as the default tenant on this daemon.
    pub fn serve(self: &Arc<Self>, listener: std::os::unix::net::UnixListener) {
        server::serve_on(self, listener);
    }

    /// Serves VM processes of `tenant` connecting to `listener` on this daemon.
    pub fn serve_tenant(
        self: &Arc<Self>,
        listener: std::os::unix::net::UnixListener,
        tenant: Arc<server::Tenant>,
    ) {
        server::serve_tenant_on(self, listener, tenant);
    }

    /// Serves the control API on `listener` for this daemon (outside `madsim`).
    #[cfg(not(madsim))]
    pub fn serve_control(
        self: &Arc<Self>,
        listener: std::os::unix::net::UnixListener,
        tenants: impl Into<PathBuf>,
    ) -> std::io::Result<()> {
        control::serve_control_on(self, listener, tenants)
    }

    /// Serves HTTP metrics and/or z-pages on `listener` for this daemon.
    pub fn serve_http(
        self: &Arc<Self>,
        listener: std::net::TcpListener,
        pages: super::Pages,
    ) -> std::io::Result<()> {
        super::http::serve_http_on(self, listener, pages)
    }

    /// Renders Prometheus metrics for this daemon.
    pub fn render_metrics(&self) -> String {
        super::metrics::render_on(self)
    }

    /// Injects `bucket` as this daemon's bucket before any cell opens (tests and `madsim`).
    #[cfg(any(test, madsim))]
    pub fn use_bucket(self: &Arc<Self>, bucket: Arc<RoutedBucket>) {
        super::storage::use_bucket_on(self, bucket);
    }
}
