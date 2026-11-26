// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! libcladefs: cladefs's extent daemon, for a process to embed (board 13471).
//!
//! machined and libmachine run one [`Daemon`] in their own process, shared by every VM the process
//! runs; `cladefsd` is a thin binary over the same daemon. A daemon serves cells from its storage
//! (local disks, a bucket behind them), its pages and caches under one memory budget:
//!
//! - [`Daemon::start`] boots one from a [`Config`], [`Daemon::stop`] drains it.
//! - [`Daemon::open`], [`Daemon::create`] and [`Daemon::fork`] give a device its cell's
//!   [`CellClient`], connected in this process: no listener, no path. The client is what an extent
//!   device serves its guest from (`virtio_extent`'s backend).
//! - [`Daemon::control_service`] is the daemon's control API (the Cladefsd gRPC service, health,
//!   and the Mount service of its FUSE mount, Linux's), for the process to serve on an endpoint of
//!   its own.
//! - [`Daemon::serve_vm_processes`] and [`Daemon::serve_vm_process`] serve VM processes over
//!   cladefs's VM-process wire, as cladefsd does: helper processes the process starts, each
//!   connecting with [`CellClient::connect`], or over a socket it was handed with
//!   [`DaemonConnection::over`] (on macOS a process runs one VM, board 13460).
//!
//! What waits for the daemon's runtime is `async` ([`Daemon::start`], [`Daemon::stop`],
//! [`Daemon::control_service`]): awaited on any executor, the process's own runtime included, or
//! on none (`futures::executor::block_on`). A [`CellClient`]'s calls block their thread, as a
//! device's worker's do.
//!
//! One daemon a process: its configuration is the process's (below), and a second
//! [`Daemon::start`] fails while one runs.
//!
//! cladefs: outside the simulation, whose daemons are booted by it (doc-24).

#![cfg(not(madsim))]
#![deny(missing_docs)]
#![deny(clippy::undocumented_unsafe_blocks)]

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub use cladefs_core::{CellId, CellVersion, Error, Result};
pub use cladefs_wire::{CellClient, DaemonConnection};
/// The gRPC crate [`Daemon::control_service`]'s routes are of, for the process's server.
pub use tonic;

/// What a daemon serves from: cladefsd's configuration, its TOML file's (`cladefsd --config`), or
/// built here. An embedded daemon needs no socket: `daemon.vm_socket` and `daemon.control_socket`
/// are served if set, as cladefsd serves them; unset, the process serves what it wants through
/// [`Daemon::serve_vm_processes`] and [`Daemon::control_service`].
#[derive(Clone, Debug)]
pub struct Config(cladefs_core::config::Config);

impl Config {
    /// The configuration that TOML `text` says, as cladefsd reads its file.
    pub fn from_toml(text: &str) -> Result<Self, Error> {
        cladefs_core::config::Config::from_toml_str(text).map(Self).map_err(config_error)
    }

    /// The configuration in the TOML file at `path`.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, Error> {
        cladefs_core::config::Config::from_toml_file(path.as_ref()).map(Self).map_err(config_error)
    }

    /// One local disk at `dir`, holding the cells, the part cache and the tenants' sockets, and a
    /// bucket of local files in it (`dir/bucket`), the host's token 1: a developer's machine's,
    /// whose bucket no other host shares. Its keys need a KMS ([`Config::kms`]) or a master key
    /// ([`Config::master_key_file`]).
    pub fn local(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let mut config = cladefs_core::config::Config::default();
        config.bucket.url = Some(format!("file://{}", dir.join("bucket").display()));
        config.daemon.token = Some(1);
        config.daemon.tenants_dir = Some(dir.join("tenants"));
        config.disk.part_cache = true;
        config.disks.push(cladefs_core::config::DiskEntryConfig {
            id: None,
            path: dir.join("disk"),
            cap: None,
            headroom: None,
            cache_cap: None,
            dirty_budget: None,
            replicas: true,
            placement: Default::default(),
        });
        Self(config)
    }

    /// Cells uploaded to the bucket at `url` (`s3://`, `gs://`, `file://`, ...).
    pub fn bucket(mut self, url: impl Into<String>) -> Self {
        self.0.bucket.url = Some(url.into());
        self
    }

    /// The KMS at `url` encrypting every tenant's keys.
    pub fn kms(mut self, url: impl Into<String>) -> Self {
        self.0.kms.url = Some(url.into());
        self
    }

    /// The master key in the file at `path` encrypting every tenant's keys, where no KMS does.
    pub fn master_key_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.0.kms.master_key_file = Some(path.into());
        self
    }

    /// The memory the daemon's pages and caches may hold, all told: its one budget. Unset, the
    /// daemon's governor sizes it from the host's memory as that changes.
    pub fn memory(mut self, bytes: u64) -> Self {
        self.0.memory.bytes = Some(cladefs_core::config::ByteSize(bytes));
        self
    }

    /// The most bytes of parts the disks' part cache holds.
    pub fn cache_cap(mut self, bytes: u64) -> Self {
        self.0.disk.cache_cap = Some(cladefs_core::config::ByteSize(bytes));
        self
    }

    /// Where the sockets of the tenants the control API grants go: `daemon.tenants_dir`, or
    /// beside the control socket, as cladefsd's, or in the first disk's directory.
    fn tenants(&self) -> Option<PathBuf> {
        let config = &self.0;
        (config.daemon.tenants_dir.clone())
            .or_else(|| config.daemon.control_socket.as_ref().map(|c| c.with_file_name("tenants")))
            .or_else(|| config.disks.first().map(|disk| disk.path.join("tenants")))
    }
}

fn config_error(why: String) -> Error {
    Error::new(anyhow::anyhow!("libcladefs: {why}"))
}

/// Whether a daemon runs in this process. The process's: one daemon is embedded in a process,
/// whose environment its configuration is (`node_local::env::set_embedded_vars`).
static RUNNING: AtomicBool = AtomicBool::new(false);

/// cladefs's extent daemon, embedded in this process (see the crate's docs). Each clone is the
/// same daemon.
#[derive(Clone)]
pub struct Daemon {
    daemon: Arc<cladefsd::Daemon>,
    /// Once it booted (it boots once): where the sockets of the tenants its control API grants go,
    /// if its configuration names one ([`Config::tenants`]).
    booted: Arc<OnceLock<Option<PathBuf>>>,
}

impl Daemon {
    /// Boots a daemon serving from `config` ([`Daemon::new`], then [`Daemon::boot`]).
    pub async fn start(config: Config) -> Result<Self, Error> {
        let daemon = Self::new()?;
        match daemon.boot(config).await {
            Ok(()) => Ok(daemon),
            Err(err) => {
                RUNNING.store(false, Ordering::Release);
                Err(err)
            }
        }
    }

    /// This process's daemon, booted nothing yet ([`Daemon::boot`]): no thread started, so that a
    /// process may block the signals it takes on a thread of its own first, as cladefsd does.
    /// Fails if one runs in this process already.
    pub fn new() -> Result<Self, Error> {
        if RUNNING.swap(true, Ordering::AcqRel) {
            return Err(Error::new(anyhow::anyhow!(
                "libcladefs: a daemon runs in this process already: one a process"
            )));
        }
        Ok(Self { daemon: cladefsd::Daemon::new(), booted: Arc::default() })
    }

    /// Boots the daemon to serve from `config`: its disks, part cache and bucket opened, its
    /// memory budget set, its governor started, and the sockets `config` names served; and this
    /// process's limit of open files raised to the most it may be, as the daemon keeps every
    /// cell's files open. Never sets the process's environment, so a process running threads may
    /// call it.
    pub async fn boot(&self, config: Config) -> Result<(), Error> {
        self.booting(config.tenants())?;
        node_local::env::set_embedded_vars(config.0.process_env());
        let daemon = Arc::clone(&self.daemon);
        self.daemon
            .run(async move { daemon.boot_embedded(config.0).await })
            .await
            .map_err(Error::new)
    }

    /// Boots the daemon as [`Daemon::boot`] does, configured by this process's environment as
    /// cladefsd is without a configuration file (its `CLADEFSD_*` variables), serving VM
    /// processes at `socket`, of `memory` bytes fixed, or governed if none.
    pub async fn boot_from_env(&self, socket: &Path, memory: Option<u64>) -> Result<(), Error> {
        self.booting(None)?;
        let (daemon, socket) = (Arc::clone(&self.daemon), socket.to_owned());
        self.daemon.run(async move { daemon.boot(&socket, memory).await }).await.map_err(Error::new)
    }

    /// Drains the daemon within `within`: every cell closed, its commits uploaded, no more served.
    /// What it could not drain in time it names in its error. Another daemon may start after.
    pub async fn stop(self, within: Duration) -> Result<(), Error> {
        let draining = Arc::clone(&self.daemon);
        let drained = self.daemon.run(async move { draining.drain(within).await }).await;
        RUNNING.store(false, Ordering::Release);
        drained.map_err(|left| {
            Error::new(anyhow::anyhow!("not drained within {}s: {left}", within.as_secs()))
        })
    }

    /// The existing managed cell `cell`, opened for a device, connected in this process.
    pub fn open(&self, cell: CellId) -> Result<CellClient, Error> {
        self.connection()?.open_cell(cell)
    }

    /// A new managed cell of `tenant`'s, of `capacity` bytes, on a disk the daemon picks, opened
    /// for a device: its ID is the client's ([`CellClient::cell`]).
    pub fn create(&self, tenant: u64, capacity: u64) -> Result<CellClient, Error> {
        self.connection()?.create_cell(tenant, capacity)
    }

    /// A new managed cell of `tenant`'s, forked from the version `from`, of `capacity` bytes,
    /// opened for a device.
    pub fn fork(&self, tenant: u64, from: CellVersion, capacity: u64) -> Result<CellClient, Error> {
        self.connection()?.open_forked_cell(tenant, from, capacity)
    }

    /// The daemon's control API, the Cladefsd gRPC service, health and its FUSE mount's Mount, for
    /// the process to serve on an endpoint of its own
    /// (`tonic::transport::Server::builder().add_routes(..)`), as cladefsd serves its control
    /// socket. The sockets of the tenants it grants go in `daemon.tenants_dir`, or the first
    /// disk's `tenants`.
    pub async fn control_service(&self) -> Result<tonic::service::Routes, Error> {
        let tenants = self.booted.get().cloned().flatten().ok_or_else(|| {
            Error::new(anyhow::anyhow!(
                "libcladefs: no directory for tenants' sockets: booted from no configuration \
                 naming disks or daemon.tenants_dir"
            ))
        })?;
        let daemon = Arc::clone(&self.daemon);
        self.daemon
            .run(async move { daemon.control_routes(tenants).await })
            .await
            .map_err(Error::new)
    }

    /// Serves VM processes connecting to a new socket at `socket`, which only this process's user
    /// may connect to, over cladefs's VM-process wire, until the daemon stops.
    pub fn serve_vm_processes(&self, socket: impl AsRef<Path>) -> Result<(), Error> {
        self.daemon.serve_vm_socket(socket.as_ref()).map_err(Error::new)
    }

    /// Serves the VM process at the other end of `stream`, an end of a socket pair the process
    /// handed a VM process it started, as one connecting to [`Daemon::serve_vm_processes`]'s
    /// socket is served.
    pub fn serve_vm_process(&self, stream: UnixStream) -> Result<(), Error> {
        self.daemon.serve_vm_process(stream).map_err(|err| Error::new(anyhow::Error::new(err)))
    }

    /// Says the daemon boots, its tenants' sockets in `tenants`, if any: refused if it booted
    /// before; and raises this process's limit of open files.
    fn booting(&self, tenants: Option<PathBuf>) -> Result<(), Error> {
        if self.booted.set(tenants).is_err() {
            return Err(Error::new(anyhow::anyhow!("libcladefs: the daemon booted already")));
        }
        cladefsd::raise_open_files_limit().map_err(|err| {
            Error::new(anyhow::Error::new(err).context("raising the limit of open files"))
        })
    }

    /// A connection of this process's to the daemon.
    fn connection(&self) -> Result<DaemonConnection, Error> {
        let stream = self.daemon.connect_in_process().map_err(|err| {
            Error::new(anyhow::Error::new(err).context("connecting to the embedded daemon"))
        })?;
        DaemonConnection::over(stream)
    }
}
