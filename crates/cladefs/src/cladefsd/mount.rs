// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the daemon's FUSE mount of tenants' cells (TASK-475.2), which `cladefs-mount
//! --control` served from a process of its own before: one directory of cells, each Fxfs cell
//! attached shown at `TENANT/CELL` (`crate::fuse::control`), its Mount service
//! served on the daemon's control socket beside Cladefsd ([`super::control`]). In the daemon, a
//! cell is held by one process, whichever serves it, VMs or the mount, with one
//! bucket client, part cache, upload budget and drain.
//!
//! The mount is made as the daemon starts, if `CLADEFSD_MOUNT` names a mount point
//! ([`Config::from_env`]), and closed as it drains ([`close`]): unmounted lazily, its
//! cells written back and closed once nothing uses them, within the drain's
//! bound. Unmounted otherwise meanwhile (`umount`), it serves no more, and the daemon goes on
//! serving VMs; its cells close as it drains.
//!
//! cladefs: under madsim, a simulated host mounts nothing, FUSE being outside the simulation
//! (doc-24): [`start`] refuses, and [`close`] has nothing to close, on macOS too, whose FUSE is
//! not Linux's (board 2912).

use crate::error::Error;
#[cfg(not(madsim))]
use crate::fuse::control::{self, CellSource};
#[cfg(not(madsim))]
use crate::fuse::{CellsMount, MountOptions};
use std::path::PathBuf;
#[cfg(not(madsim))]
use std::sync::Mutex;
#[cfg(not(madsim))]
use std::sync::atomic::{AtomicBool, Ordering};

/// Where the daemon mounts its cells, and keeps their cell directories.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The mount point, a directory: `CLADEFSD_MOUNT`.
    pub mountpoint: PathBuf,
    /// The directory the attached cell directories are in, each at `TENANT/CELL`:
    /// `CLADEFSD_MOUNT_STORES`.
    pub cell_dirs: PathBuf,
    /// Whether users other than the daemon's may use the mount, as its files' permissions say
    /// (FUSE's `allow_other`): `CLADEFSD_MOUNT_ALLOW_OTHER=1`.
    pub allow_other: bool,
}

impl Config {
    /// The mount the environment names: none without `CLADEFSD_MOUNT`.
    pub fn from_env() -> Result<Option<Self>, String> {
        // Unset, or not UTF-8, which no value it takes is: none.
        Self::from_vars(|name| node_local::env::var(name).ok().filter(|value| !value.is_empty()))
    }

    /// The mount the variables `var` gives name, as [`Config::from_env`] reads them.
    fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, String> {
        let Some(mountpoint) = var("CLADEFSD_MOUNT") else {
            for alone in ["CLADEFSD_MOUNT_STORES", "CLADEFSD_MOUNT_ALLOW_OTHER"] {
                if var(alone).is_some() {
                    return Err(format!("{alone} is set, but CLADEFSD_MOUNT names no mount point"));
                }
            }
            return Ok(None);
        };
        let cell_dirs = var("CLADEFSD_MOUNT_STORES").ok_or_else(|| {
            "CLADEFSD_MOUNT is set, but CLADEFSD_MOUNT_STORES names no directory for its \
             cell directories"
                .to_owned()
        })?;
        let allow_other = match var("CLADEFSD_MOUNT_ALLOW_OTHER").as_deref() {
            None | Some("0") => false,
            Some("1") => true,
            Some(other) => {
                return Err(format!("CLADEFSD_MOUNT_ALLOW_OTHER is 0 or 1, not {other}"));
            }
        };
        Ok(Some(Self { mountpoint: mountpoint.into(), cell_dirs: cell_dirs.into(), allow_other }))
    }
}

/// The mount, and where its cells come from: from [`start`] until [`close`] takes it.
#[cfg(not(madsim))]
static MOUNT: Mutex<Option<(CellsMount, CellSource)>> = Mutex::new(None);

/// Whether the daemon mounted its cells ([`start`]).
#[cfg(not(madsim))]
static MOUNTED: AtomicBool = AtomicBool::new(false);

/// Whether [`close`] closed the mount, its cells written back.
#[cfg(not(madsim))]
static CLOSED: AtomicBool = AtomicBool::new(false);

/// Mounts the daemon's directory of cells as `config` says, its cells taken from the bucket at
/// `bucket`, the host's: before the control API serves, whose Mount service it is ([`service`]).
#[cfg(not(madsim))]
pub fn start(config: &Config, bucket: String) -> Result<(), Error> {
    std::fs::create_dir_all(&config.cell_dirs).map_err(|err| {
        Error::new(
            anyhow::Error::new(err).context(format!("making {}", config.cell_dirs.display())),
        )
    })?;
    let mut options = MountOptions::new();
    options.allow_other(config.allow_other);
    let mount = options.mount_cells(&config.mountpoint).map_err(|err| {
        err.context(format!("mounting a directory of cells at {}", config.mountpoint.display()))
    })?;
    log::info!(
        "cladefsd: cells mounted at {}, their cell directories in {}",
        config.mountpoint.display(),
        config.cell_dirs.display()
    );
    let cells = CellSource { dir: config.cell_dirs.clone(), bucket };
    let replaced = MOUNT.lock().unwrap().replace((mount, cells));
    assert!(replaced.is_none(), "the daemon mounts its cells once");
    MOUNTED.store(true, Ordering::Release);
    Ok(())
}

/// The Mount service of the daemon's mount, if it has one ([`start`]). cladefs: none under madsim,
/// FUSE being outside the simulation (doc-24).
#[cfg(not(madsim))]
pub(super) fn service() -> Option<
    control::proto::mount_server::MountServer<impl control::proto::mount_server::Mount + use<>>,
> {
    let mount = MOUNT.lock().unwrap();
    let (mount, cells) = mount.as_ref()?;
    Some(control::service(mount.server(), cells.clone()))
}

/// cladefs: under madsim, refused: a simulated host mounts no cells, FUSE being outside the
/// simulation (doc-24).
#[cfg(madsim)]
pub fn start(config: &Config, _: String) -> Result<(), Error> {
    Err(Error::new(anyhow::anyhow!(
        "a simulated host mounts no cells at {}: FUSE is outside the simulation",
        config.mountpoint.display()
    )))
}

/// Whether the daemon mounted its cells and has not closed them, as [`close`] does, or failed to:
/// what a drain past its bound left.
pub(super) fn left() -> bool {
    #[cfg(not(madsim))]
    return MOUNTED.load(Ordering::Acquire) && !CLOSED.load(Ordering::Acquire);
    #[cfg(madsim)]
    false
}

/// Closes the daemon's mount, if it has one, as it drains: unmounted lazily, so at once from its
/// mount point, and once nothing uses it, its cells written back and closed. Holds
/// no thread of the runtime while it waits.
#[cfg(not(madsim))]
pub(super) async fn close() -> Result<(), Error> {
    let Some((mount, _)) = MOUNT.lock().unwrap().take() else { return Ok(()) };
    let mountpoint = mount.mountpoint().to_owned();
    // cladefs: outside the simulation: FUSE (doc-24).
    let closed = tokio::task::spawn_blocking(move || {
        mount.unmount(true)?;
        mount.join()
    });
    closed.await.map_err(Error::new)?.map_err(|err| {
        err.context(format!("closing the cells mounted at {}", mountpoint.display()))
    })?;
    CLOSED.store(true, Ordering::Release);
    log::info!("cladefsd: the cells mounted at {} closed", mountpoint.display());
    Ok(())
}

/// cladefs: under madsim, nothing to close: a simulated host mounts no cells ([`start`]).
#[cfg(madsim)]
pub(super) async fn close() -> Result<(), Error> {
    Ok(())
}
