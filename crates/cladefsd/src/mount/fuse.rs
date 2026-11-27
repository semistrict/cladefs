// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the daemon's FUSE mount of cells as a daemon built with its `mount` feature has it, on
//! Linux, outside the simulation ([`super`]): `cladefs_fuse`'s mount of a directory of cells, and
//! the Mount service that attaches them ([`crate::fuse_control`]).

use super::Config;
use crate::Daemon;
use crate::fuse_control::{self as control, CellSource};
use cladefs_core::error::Error;
pub(crate) use cladefs_fuse::Counts;
use cladefs_fuse::{CellsMount, MountOptions};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// The service the mount's reader threads are, as the daemon's log names it when a panic on one
/// aborts the daemon (board 12443, hotbed question 561).
const SERVICE: &str = "cladefsd fuse";

/// Per-daemon FUSE mount state, owned by [`Daemon`].
#[derive(Default)]
pub(crate) struct State {
    /// The mount, and where its cells come from: from [`start_on`] until [`close_on`] takes it.
    mount: Mutex<Option<(CellsMount, CellSource)>>,
    /// Whether the daemon mounted its cells ([`start_on`]).
    mounted: AtomicBool,
    /// Whether [`close_on`] closed the mount, its cells written back.
    closed: AtomicBool,
    /// Bytes of FUSE requests currently queued in per-tenant admission (board 9850), counted by
    /// the daemon's memory governor without locking `mount`.
    queued_bytes: Arc<AtomicU64>,
}

/// Mounts `daemon`'s directory of cells as `config` says, its cells taken from the bucket at
/// `bucket`, the host's: before the control API serves, whose Mount service it is ([`service_on`]).
pub(crate) fn start_on(daemon: &Daemon, config: &Config, bucket: String) -> Result<(), Error> {
    for dir in &config.cell_dirs {
        std::fs::create_dir_all(dir).map_err(|err| {
            Error::new(anyhow::Error::new(err).context(format!("making {}", dir.display())))
        })?;
    }
    let mut options = MountOptions::new();
    options
        .allow_other(config.allow_other)
        .threads(MountOptions::default_threads())
        .queued_bytes_counter(Arc::clone(&daemon.mount.queued_bytes))
        .supervised(SERVICE);
    let mount = options.mount_cells(&config.mountpoint).map_err(|err| {
        err.context(format!("mounting a directory of cells at {}", config.mountpoint.display()))
    })?;
    let shown =
        config.cell_dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(":");
    log::info!(
        "cladefsd: cells mounted at {}, their cell directories in {shown}",
        config.mountpoint.display(),
    );
    let cells = CellSource { dirs: config.cell_dirs.clone(), bucket };
    let replaced = daemon.mount.mount.lock().unwrap().replace((mount, cells));
    assert!(replaced.is_none(), "the daemon mounts its cells once");
    daemon.mount.mounted.store(true, Ordering::Release);
    Ok(())
}

/// The Mount service of `daemon`'s mount, if it has one ([`start_on`]).
pub(crate) fn service_on(
    daemon: &Arc<Daemon>,
) -> Option<
    control::proto::mount_server::MountServer<impl control::proto::mount_server::Mount + use<>>,
> {
    let mount = daemon.mount.mount.lock().unwrap();
    let (mount, cells) = mount.as_ref()?;
    Some(control::service_on(Arc::clone(daemon), mount.server(), cells.clone()))
}

/// cladefs: what `daemon`'s mount of cells holds and did, for its metrics (board 8565): its cells
/// attached, those being detached, and its server's counts; nothing, all 0, without a mount, or
/// once it closed.
pub(crate) fn counts_on(daemon: &Daemon) -> (usize, usize, Counts) {
    if let Some((mount, _)) = daemon.mount.mount.lock().unwrap().as_ref() {
        let server = mount.server();
        let attached = server.attached();
        let detaching = attached.iter().filter(|cell| cell.detaching).count();
        return (attached.len() - detaching, detaching, server.counts());
    }
    (0, 0, Counts::default())
}

/// Bytes of FUSE requests currently queued in per-tenant admission on `daemon` (board 9850).
pub(crate) fn queued_bytes_on(daemon: &Daemon) -> u64 {
    daemon.mount.queued_bytes.load(Ordering::Relaxed)
}

/// Whether `daemon` mounted its cells and has not closed them, as [`close_on`] does, or failed to:
/// what a drain past its bound left.
pub(crate) fn left_on(daemon: &Daemon) -> bool {
    daemon.mount.mounted.load(Ordering::Acquire) && !daemon.mount.closed.load(Ordering::Acquire)
}

/// Closes `daemon`'s mount, if it has one, as it drains: unmounted lazily, so at once from its
/// mount point, and once nothing uses it, its cells written back and closed. Holds
/// no thread of the runtime while it waits.
pub(crate) async fn close_on(daemon: &Daemon) -> Result<(), Error> {
    let Some((mount, _)) = daemon.mount.mount.lock().unwrap().take() else { return Ok(()) };
    let mountpoint = mount.mountpoint().to_owned();
    // cladefs: outside the simulation: FUSE (doc-24).
    let closed = tokio::task::spawn_blocking(move || {
        host_fs::may_block(|| {
            mount.unmount(true)?;
            mount.join()
        })
    });
    closed.await.map_err(Error::new)?.map_err(|err| {
        err.context(format!("closing the cells mounted at {}", mountpoint.display()))
    })?;
    daemon.mount.closed.store(true, Ordering::Release);
    log::info!("cladefsd: the cells mounted at {} closed", mountpoint.display());
    Ok(())
}

/// The cells mounted in this process and not halted, which a shredding the daemon missed the
/// event of is reconciled against (`crate::events`, board 11665).
pub(crate) fn live_cells() -> Vec<cladefs_core::CellId> {
    cladefs_fuse::server::live_cells()
}
