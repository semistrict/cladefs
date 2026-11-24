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

use super::Daemon;
use crate::error::Error;
#[cfg(not(madsim))]
use crate::fuse::control::{self, CellSource};
#[cfg(not(madsim))]
use crate::fuse::{CellsMount, MountOptions};
use std::path::PathBuf;
#[cfg(not(madsim))]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(not(madsim))]
use std::sync::{Arc, Mutex};

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

/// Per-daemon FUSE mount state, owned by [`Daemon`].
#[cfg(not(madsim))]
#[derive(Default)]
pub(super) struct State {
    /// The mount, and where its cells come from: from [`start_on`] until [`close_on`] takes it.
    mount: Mutex<Option<(CellsMount, CellSource)>>,
    /// Whether the daemon mounted its cells ([`start_on`]).
    mounted: AtomicBool,
    /// Whether [`close_on`] closed the mount, its cells written back.
    closed: AtomicBool,
}

/// Mounts `daemon`'s directory of cells as `config` says, its cells taken from the bucket at
/// `bucket`, the host's: before the control API serves, whose Mount service it is ([`service_on`]).
#[cfg(not(madsim))]
pub(super) fn start_on(daemon: &Daemon, config: &Config, bucket: String) -> Result<(), Error> {
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
    let replaced = daemon.mount.mount.lock().unwrap().replace((mount, cells));
    assert!(replaced.is_none(), "the daemon mounts its cells once");
    daemon.mount.mounted.store(true, Ordering::Release);
    Ok(())
}

/// Mounts the process's default daemon's directory of cells.
#[cfg(not(madsim))]
pub fn start(config: &Config, bucket: String) -> Result<(), Error> {
    start_on(Daemon::process(), config, bucket)
}

/// The Mount service of `daemon`'s mount, if it has one ([`start_on`]). cladefs: none under madsim,
/// FUSE being outside the simulation (doc-24).
#[cfg(not(madsim))]
pub(super) fn service_on(
    daemon: &Arc<Daemon>,
) -> Option<
    control::proto::mount_server::MountServer<impl control::proto::mount_server::Mount + use<>>,
> {
    let mount = daemon.mount.mount.lock().unwrap();
    let (mount, cells) = mount.as_ref()?;
    Some(control::service_on(Arc::clone(daemon), mount.server(), cells.clone()))
}

/// cladefs: under madsim, refused: a simulated host mounts no cells, FUSE being outside the
/// simulation (doc-24).
#[cfg(madsim)]
pub(super) fn start_on(_daemon: &Daemon, config: &Config, _: String) -> Result<(), Error> {
    Err(Error::new(anyhow::anyhow!(
        "a simulated host mounts no cells at {}: FUSE is outside the simulation",
        config.mountpoint.display()
    )))
}

/// cladefs: what `daemon`'s mount of cells holds and did, for its metrics (board 8565): its cells
/// attached, those being detached, and its server's counts; nothing, all 0, without a mount, or
/// once it closed, as under madsim, where a simulated host mounts none.
pub(super) fn counts_on(daemon: &Daemon) -> (usize, usize, crate::fuse::Counts) {
    #[cfg(not(madsim))]
    if let Some((mount, _)) = daemon.mount.mount.lock().unwrap().as_ref() {
        let server = mount.server();
        let attached = server.attached();
        let detaching = attached.iter().filter(|cell| cell.detaching).count();
        return (attached.len() - detaching, detaching, server.counts());
    }
    #[cfg(madsim)]
    let _ = daemon;
    (0, 0, crate::fuse::Counts::default())
}

/// Whether `daemon` mounted its cells and has not closed them, as [`close_on`] does, or failed to:
/// what a drain past its bound left.
pub(super) fn left_on(daemon: &Daemon) -> bool {
    #[cfg(not(madsim))]
    return daemon.mount.mounted.load(Ordering::Acquire)
        && !daemon.mount.closed.load(Ordering::Acquire);
    #[cfg(madsim)]
    {
        let _ = daemon;
        false
    }
}

/// Closes `daemon`'s mount, if it has one, as it drains: unmounted lazily, so at once from its
/// mount point, and once nothing uses it, its cells written back and closed. Holds
/// no thread of the runtime while it waits.
#[cfg(not(madsim))]
pub(super) async fn close_on(daemon: &Daemon) -> Result<(), Error> {
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

/// cladefs: under madsim, nothing to close: a simulated host mounts no cells ([`start_on`]).
#[cfg(madsim)]
pub(super) async fn close_on(_daemon: &Daemon) -> Result<(), Error> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(vars: &[(&str, &str)]) -> Result<Option<Config>, String> {
        Config::from_vars(|name| {
            vars.iter().find(|(set, _)| *set == name).map(|(_, value)| (*value).to_owned())
        })
    }

    #[test]
    fn a_mount_point_needs_a_directory_of_cells() {
        assert_eq!(
            config(&[("CLADEFSD_MOUNT", "/m")]),
            Err("CLADEFSD_MOUNT is set, but CLADEFSD_MOUNT_STORES names no directory for its \
                 cell directories"
                .to_owned())
        );
        assert_eq!(
            config(&[("CLADEFSD_MOUNT", "/m"), ("CLADEFSD_MOUNT_STORES", "/s")]),
            Ok(Some(Config {
                mountpoint: "/m".into(),
                cell_dirs: "/s".into(),
                allow_other: false
            }))
        );
        assert_eq!(
            config(&[
                ("CLADEFSD_MOUNT", "/m"),
                ("CLADEFSD_MOUNT_STORES", "/s"),
                ("CLADEFSD_MOUNT_ALLOW_OTHER", "1")
            ]),
            Ok(Some(Config { mountpoint: "/m".into(), cell_dirs: "/s".into(), allow_other: true }))
        );
    }

    #[test]
    fn mount_settings_without_a_mount_point_are_refused() {
        assert_eq!(
            config(&[("CLADEFSD_MOUNT_STORES", "/s")]),
            Err("CLADEFSD_MOUNT_STORES is set, but CLADEFSD_MOUNT names no mount point".to_owned())
        );
        assert_eq!(
            config(&[
                ("CLADEFSD_MOUNT", "/m"),
                ("CLADEFSD_MOUNT_STORES", "/s"),
                ("CLADEFSD_MOUNT_ALLOW_OTHER", "yes")
            ]),
            Err("CLADEFSD_MOUNT_ALLOW_OTHER is 0 or 1, not yes".to_owned())
        );
    }
}
