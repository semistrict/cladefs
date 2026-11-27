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
use crate::control;
use crate::{cells, disk, drain, governor, memory, migration, peers, pool, process, server, stats};
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
    pub(crate) host: Arc<cladefs_core::Host>,
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
    /// cladefs: cells open in the daemon's process (`embedded`), counted for drain as connections
    /// are: a drain waits for their holders to let them go, and refuses more (board 13909).
    pub(super) embedded_work: drain::Work,
    /// Consolidated Tokio runtime for VM socket I/O, control gRPC, peer gRPC, and memory governor.
    /// Under `madsim`, tasks spawn directly onto the simulated node's runtime.
    #[cfg(not(madsim))]
    runtime: OnceLock<tokio::runtime::Runtime>,
    /// Open cell registry and holds (`CellTable`).
    pub(super) table: cells::Table,
    /// Per-cell uploader interval overrides configured via `CreateCellRequest` (hotbed question
    /// 534), keyed by canonical cell directory path and cleared when the cell is deleted/dropped.
    pub(super) upload_intervals: Mutex<std::collections::HashMap<PathBuf, std::time::Duration>>,
    /// The daemon's bucket handle, initialized on first use or injected in tests.
    pub(super) bucket: OnceLock<Arc<RoutedBucket>>,
    /// The daemon's appendable zonal fast WAL bucket and large-extent threshold
    /// (`CLADEFSD_FAST_WAL_BUCKET`, `CLADEFSD_FAST_WAL_LARGE_EXTENT_BYTES`, doc-29).
    pub(super) fast_wal: Mutex<Option<Option<(local_cell::fast_wal::FastWalConfig, u64)>>>,
    /// The daemon's part cache, opened once from `CLADEFSD_PART_CACHE`.
    pub(super) part_cache: Mutex<Option<Option<Arc<Cache>>>>,
    /// Listening VM socket FDs, and what stops their accepts, stopped on drain.
    pub(super) accepting: Mutex<Vec<(std::os::fd::RawFd, Arc<server::StopAccepting>)>>,
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
    /// Per-thread active quarantine waiters for [`server::Parking::counting`] (board 11703).
    pub(super) parking: Mutex<Vec<(std::thread::ThreadId, Arc<server::Waiting>)>>,
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
    /// Managed JBOD disk pool (`[[disks]]`), cell-ID index, and per-disk governance (hotbed
    /// question 547).
    pub(super) disk_pool: OnceLock<Arc<crate::disks::DiskPool>>,
    /// Mutual TLS credentials for peer gRPC (`CLADEFSD_PEER_TLS`).
    pub(super) peer_credentials: Mutex<Option<peers::Credentials>>,
    /// Shared HTTP/2 peer gRPC channel pool across GetPart and migration (board
    /// 11713, board 11659).
    pub(super) peer_channels: OnceLock<peers::Channels>,
    /// Highest `PeersUpdated` generation installed locally on this daemon, serialized with peer
    /// list updates so a racing `SetPeers` RPC cannot overwrite a newer polled generation
    /// (hotbed review 140 finding 520).
    pub(super) peers_generation: Mutex<u64>,
    /// Outgoing and incoming ephemeral cell migration state.
    pub(super) migrations: migration::State,
    /// Active VM connection reports for `GetStats` and `/metrics`.
    pub(super) stats_reports: Mutex<Vec<Weak<stats::Report>>>,
    /// Process memory budget and RSS/CPU sample state.
    pub(super) process: Mutex<process::State>,
    /// Pages dirtied writing the periodic stats file (`CLADEFSD_STATS`).
    pub(super) stats_written: AtomicU64,
    /// FUSE mount state (Linux's, outside `madsim`).
    #[cfg(all(target_os = "linux", not(madsim)))]
    pub(super) mount: crate::mount::State,
    /// Number of times `kept_or_open` opened a version/kept cell store (TASK-549, board 2494).
    #[cfg(test)]
    pub(super) kept_opens: AtomicU64,
}

impl Daemon {
    /// Creates a fresh extent daemon instance with an isolated [`crate::Host`], empty registries,
    /// and its own consolidated runtime (created lazily on first async use outside `madsim`).
    pub fn new() -> Arc<Self> {
        Self::with_host(cladefs_core::Host::new())
    }

    /// Creates a fresh extent daemon instance backed by `host`.
    pub fn with_host(host: Arc<cladefs_core::Host>) -> Arc<Self> {
        let daemon = Arc::new(Self {
            host,
            serving: watch::channel(false).0,
            reported: watch::channel(false).0,
            control_serving: AtomicBool::new(false),
            draining: watch::channel(false).0,
            connections_work: drain::Work::new(),
            calls_work: drain::Work::new(),
            embedded_work: drain::Work::new(),
            #[cfg(not(madsim))]
            runtime: OnceLock::new(),
            table: cells::Table::new(),
            upload_intervals: Mutex::new(std::collections::HashMap::new()),
            bucket: OnceLock::new(),
            fast_wal: Mutex::new(None),
            part_cache: Mutex::new(None),
            accepting: Mutex::new(Vec::new()),
            connected: Mutex::new(0),
            default_tenant: OnceLock::new(),
            roots: OnceLock::new(),
            flush_turns: Semaphore::new(server::FLUSHES),
            vm_peers: Mutex::new(Vec::new()),
            parking: Mutex::new(Vec::new()),
            watching_vm_peers: Once::new(),
            wedged: AtomicU64::new(0),
            pool: OnceLock::new(),
            read_traces: OnceLock::new(),
            memory: memory::State::new(),
            governor: governor::State::default(),
            disk: disk::State::new(),
            disk_pool: OnceLock::new(),
            peer_credentials: Mutex::new(None),
            peer_channels: OnceLock::new(),
            peers_generation: Mutex::new(0),
            migrations: migration::State::new(),
            stats_reports: Mutex::new(Vec::new()),
            process: process::State::new(),
            stats_written: AtomicU64::new(0),
            #[cfg(all(target_os = "linux", not(madsim)))]
            mount: crate::mount::State::default(),
            #[cfg(test)]
            kept_opens: AtomicU64::new(0),
        });
        server::register_daemon(&daemon);
        daemon
    }

    /// The per-host context backing this daemon.
    pub fn host(&self) -> &Arc<cladefs_core::Host> {
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
        PROCESS.get_or_init(|| Self::with_host(cladefs_core::Host::process().clone()))
    }

    /// The daemon's consolidated Tokio runtime (outside `madsim`), shared by VM socket I/O,
    /// control gRPC, peer gRPC, the memory governor, and FUSE-mounted Fxfs cells.
    #[cfg(not(madsim))]
    pub(crate) fn runtime(&self) -> &tokio::runtime::Runtime {
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

    /// cladefs: runs `future` on this daemon's runtime, which the returned future waits for,
    /// on whatever executor, or none (`futures::executor::block_on`): what a process embedding the
    /// daemon awaits of it from its own runtime, where [`Daemon::block_on`] would panic (board
    /// 13471). A panic of `future`'s is the caller's again.
    #[cfg(not(madsim))]
    pub async fn run<F>(&self, future: F) -> F::Output
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        match self.spawn(future).await {
            Ok(output) => output,
            Err(failed) if failed.is_panic() => std::panic::resume_unwind(failed.into_panic()),
            // Cancelled: only the runtime's going cancels it, which only the daemon's does.
            Err(failed) => panic!("the daemon's runtime went, running a caller's work: {failed}"),
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

    /// The managed JBOD disk pool configured for this daemon, if initialized.
    pub(crate) fn disk_pool(&self) -> Option<&Arc<crate::disks::DiskPool>> {
        self.disk_pool.get()
    }

    /// Returns the daemon-owned host directory for `cell` in this daemon's [`super::disks::DiskPool`],
    /// if configured and known (for white-box tests inspecting encryption at rest or footprint).
    pub fn cell_path(&self, cell: cladefs_core::CellId) -> Option<PathBuf> {
        self.disk_pool.get()?.find_cell(cell)
    }

    /// Checks whether the daemon's [`crate::disks::DiskPool`] (if configured) currently has a
    /// writable disk that accepts new cells (`ENOSPC` if all disks are draining/read-only/full).
    // cladefs: the FUSE mount's (`fuse_control`): Linux's.
    #[cfg(all(target_os = "linux", not(madsim)))]
    pub(crate) fn check_disk_placement(&self) -> Result<(), cladefs_core::Errno> {
        if let Some(pool) = self.disk_pool.get() {
            drop(pool.pick_disk()?);
        }
        Ok(())
    }

    /// Resolves `cell`'s existing path in [`crate::disks::DiskPool`], or picks an eligible
    /// `ReadWrite` disk and prepares `<disk>/cells/<TENANT>/<CELL>`. Returns `Ok(None)` when no
    /// managed disk pool is configured (a daemon booted of `CLADEFSD_MOUNT_STORES` alone).
    // cladefs: the FUSE mount's (`fuse_control`): Linux's.
    #[cfg(all(target_os = "linux", not(madsim)))]
    pub(crate) fn resolve_or_pick_cell_path(
        &self,
        cell: cladefs_core::CellId,
    ) -> Result<Option<PathBuf>, cladefs_core::Errno> {
        let Some(pool) = self.disk_pool.get() else {
            return Ok(None);
        };
        if let Some(existing) = pool.find_cell(cell) {
            return Ok(Some(existing));
        }
        let disk = pool.pick_disk()?;
        drop(crate::disks::DiskPool::ensure_tenant_dir(&disk, cell.tenant)?);
        Ok(Some(crate::disks::DiskPool::cell_path(&disk, cell)))
    }

    /// Registers `cell` at `path` in this daemon's [`crate::disks::DiskPool`], if configured.
    // cladefs: the FUSE mount's (`fuse_control`): Linux's.
    #[cfg(all(target_os = "linux", not(madsim)))]
    pub(crate) fn register_cell_path(&self, cell: cladefs_core::CellId, path: PathBuf) {
        if let Some(pool) = self.disk_pool.get() {
            pool.register_cell(cell, path);
        }
    }

    /// Starts the disk governor for this daemon instance with `config`.
    pub fn start_disk_governor(
        self: &Arc<Self>,
        config: crate::disk::DiskConfig,
    ) -> Result<(), String> {
        crate::disk::start_on(self, config)
    }

    /// Initializes the daemon's managed [`super::disks::DiskPool`] from `roots` (for tests or
    /// embedded daemons that serve the control API without calling [`Self::boot_with_config`]).
    pub fn configure_disks(&self, roots: &[PathBuf]) -> Result<(), String> {
        let pool = Arc::new(crate::disks::DiskPool::from_roots(self.host.bucket_host(), roots)?);
        let cell_roots: Arc<[PathBuf]> = pool.cell_roots().into();
        // OnceLock: repeated test calls keep the first configured roots.
        let _ = self.roots.set(Ok(Some(cell_roots)));
        // OnceLock: repeated test calls keep the first configured disk pool.
        let _ = self.disk_pool.set(pool);
        Ok(())
    }

    /// Boots this daemon serving VM processes at `socket`, with `memory` bytes fixed or governed.
    pub async fn boot(self: &Arc<Self>, socket: &Path, memory: Option<u64>) -> anyhow::Result<()> {
        crate::boot::boot_daemon(self, socket, memory).await
    }

    /// Boots this daemon from a validated TOML [`crate::config::Config`] (hotbed question 547).
    pub async fn boot_with_config(
        self: &Arc<Self>,
        config: cladefs_core::config::Config,
    ) -> anyhow::Result<()> {
        crate::boot::boot_with_config(self, config).await
    }

    /// cladefs: boots this daemon from a validated [`cladefs_core::config::Config`], embedded in
    /// this process (libcladefs, board 13471): as [`Daemon::boot_with_config`], but serving VM
    /// processes at `daemon.vm_socket` only if it names one; the process connects its own devices
    /// ([`Daemon::connect_in_process`]) and hands it others ([`Daemon::serve_vm_process`]).
    pub async fn boot_embedded(
        self: &Arc<Self>,
        config: cladefs_core::config::Config,
    ) -> anyhow::Result<()> {
        crate::boot::boot_embedded(self, config).await
    }

    /// cladefs: a connection of this process's own to the daemon, of its default tenant, served
    /// as a VM process's is: an end of an anonymous socket pair, which a
    /// [`crate::DaemonConnection`] opens a cell over (board 13471).
    #[cfg(not(madsim))]
    pub fn connect_in_process(self: &Arc<Self>) -> std::io::Result<std::os::unix::net::UnixStream> {
        let tenant = server::default_tenant_on(self).map_err(std::io::Error::other)?;
        server::connect_in_memory(tenant)
    }

    /// cladefs: serves the VM process `child` at the other end of `stream`, of the default tenant,
    /// as one connecting to its socket is: an end of a socket pair this process handed a VM
    /// process it started, its child, which it must not reap before the connection ends, so that
    /// its pid names it (board 13460).
    #[cfg(not(madsim))]
    pub fn serve_vm_process(
        self: &Arc<Self>,
        stream: std::os::unix::net::UnixStream,
        child: libc::pid_t,
    ) -> std::io::Result<()> {
        server::serve_stream_on(self, stream, child).map_err(std::io::Error::other)
    }

    /// cladefs: serves VM processes connecting to a new socket at `socket`, of the default
    /// tenant, which only this process's user may connect to (TASK-155), on a thread of its own,
    /// until the daemon drains: as cladefsd serves its VM socket.
    #[cfg(not(madsim))]
    pub fn serve_vm_socket(self: &Arc<Self>, socket: &Path) -> anyhow::Result<()> {
        use anyhow::Context;
        let listener = crate::bind_owner_only(socket)
            .with_context(|| format!("listening at {}", socket.display()))?;
        let serving = Arc::clone(self);
        crate::run::thread("cladefsd vms", move || server::serve_on(&serving, listener))?;
        Ok(())
    }

    /// cladefs: the daemon's control API, its services (Cladefsd, health, its FUSE mount's
    /// Mount), for a process embedding it to serve on an endpoint of its own; the sockets of the
    /// tenants it grants in the directory `tenants`, made if it isn't there (board 13471).
    #[cfg(not(madsim))]
    pub async fn control_routes(
        self: &Arc<Self>,
        tenants: PathBuf,
    ) -> anyhow::Result<tonic::service::Routes> {
        use anyhow::Context;
        let made = tenants.clone();
        local_cell::upload::blocking(move || host_fs::create_dir_all_durably(&made))
            .with_context(|| format!("making {}", tenants.display()))?;
        Ok(control::routes(Arc::clone(self), tenants).await)
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
        match server::StopAccepting::once_made() {
            Ok(stop) => server::serve_tenant_on(self, listener, tenant, stop),
            Err(err) => log::error!("cladefsd: serving a tenant's VM socket: {err}: serving none"),
        }
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
        pages: crate::Pages,
    ) -> std::io::Result<()> {
        crate::http::serve_http_on(self, listener, pages)
    }

    /// Injects `bucket` as this daemon's bucket before any cell opens (tests and `madsim`).
    #[cfg(any(test, madsim))]
    pub fn use_bucket(self: &Arc<Self>, bucket: Arc<RoutedBucket>) {
        crate::storage::use_bucket_on(self, bucket);
    }
}
