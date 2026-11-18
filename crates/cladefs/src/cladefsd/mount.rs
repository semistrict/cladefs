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
//! ([`Config::from_env`]), and closed as it drains ([`close`]): unmounted lazily, its cells'
//! cells written back and their stores closed once nothing uses them, within the drain's
//! bound. Unmounted otherwise meanwhile (`umount`), it serves no more, and the daemon goes on
//! serving VMs; its stores close as it drains.

use crate::error::Error;
use crate::fuse::control::{self, Stores};
use crate::fuse::{MountOptions, StoresMount};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// Where the daemon mounts its cells, and keeps their cells' stores.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The mount point, a directory: `CLADEFSD_MOUNT`.
    pub mountpoint: PathBuf,
    /// The directory the attached cells' stores are in, each at `TENANT/CELL`:
    /// `CLADEFSD_MOUNT_STORES`.
    pub stores: PathBuf,
    /// Whether users other than the daemon's may use the mount, as its files' permissions say
    /// (FUSE's `allow_other`): `CLADEFSD_MOUNT_ALLOW_OTHER=1`.
    pub allow_other: bool,
}

impl Config {
    /// The mount the environment names: none without `CLADEFSD_MOUNT`.
    pub fn from_env() -> Result<Option<Self>, String> {
        Self::from_vars(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
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
        let stores = var("CLADEFSD_MOUNT_STORES").ok_or_else(|| {
            "CLADEFSD_MOUNT is set, but CLADEFSD_MOUNT_STORES names no directory for its \
             cells' stores"
                .to_owned()
        })?;
        let allow_other = match var("CLADEFSD_MOUNT_ALLOW_OTHER").as_deref() {
            None | Some("0") => false,
            Some("1") => true,
            Some(other) => {
                return Err(format!("CLADEFSD_MOUNT_ALLOW_OTHER is 0 or 1, not {other}"));
            }
        };
        Ok(Some(Self { mountpoint: mountpoint.into(), stores: stores.into(), allow_other }))
    }
}

/// The mount, and where its cells come from: from [`start`] until [`close`] takes it.
static MOUNT: Mutex<Option<(StoresMount, Stores)>> = Mutex::new(None);

/// Whether the daemon mounted its cells ([`start`]).
static MOUNTED: AtomicBool = AtomicBool::new(false);

/// Whether [`close`] closed the mount, its cells written back.
static CLOSED: AtomicBool = AtomicBool::new(false);

/// Mounts the daemon's directory of cells as `config` says, its cells taken from the bucket at
/// `bucket`, the host's: before the control API serves, whose Mount service it is ([`service`]).
pub fn start(config: &Config, bucket: String) -> Result<(), Error> {
    std::fs::create_dir_all(&config.stores).map_err(|err| {
        Error::new(anyhow::Error::new(err).context(format!("making {}", config.stores.display())))
    })?;
    let mut options = MountOptions::new();
    options.allow_other(config.allow_other);
    let mount = options.mount_stores(&config.mountpoint).map_err(|err| {
        err.context(format!("mounting a directory of cells at {}", config.mountpoint.display()))
    })?;
    log::info!(
        "cladefsd: cells mounted at {}, their stores in {}",
        config.mountpoint.display(),
        config.stores.display()
    );
    let stores = Stores { dir: config.stores.clone(), bucket };
    let replaced = MOUNT.lock().unwrap().replace((mount, stores));
    assert!(replaced.is_none(), "the daemon mounts its cells once");
    MOUNTED.store(true, Ordering::Release);
    Ok(())
}

/// The Mount service of the daemon's mount, if it has one ([`start`]).
pub(super) fn service() -> Option<
    control::proto::mount_server::MountServer<impl control::proto::mount_server::Mount + use<>>,
> {
    let mount = MOUNT.lock().unwrap();
    let (mount, stores) = mount.as_ref()?;
    Some(control::service(mount.server(), stores.clone()))
}

/// Whether the daemon mounted its cells and has not closed them, as [`close`] does, or failed to:
/// what a drain past its bound left.
pub(super) fn left() -> bool {
    MOUNTED.load(Ordering::Acquire) && !CLOSED.load(Ordering::Acquire)
}

/// Closes the daemon's mount, if it has one, as it drains: unmounted lazily, so at once from its
/// mount point, and once nothing uses it, its cells written back and their stores closed. Holds
/// no thread of the runtime while it waits.
pub(super) async fn close() -> Result<(), Error> {
    let Some((mount, _)) = MOUNT.lock().unwrap().take() else { return Ok(()) };
    let mountpoint = mount.mountpoint().to_owned();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config(vars: &[(&str, &str)]) -> Result<Option<Config>, String> {
        Config::from_vars(|name| {
            vars.iter().find(|(set, _)| *set == name).map(|(_, value)| (*value).to_owned())
        })
    }

    #[test]
    fn no_mount_point_mounts_nothing() {
        assert_eq!(config(&[]), Ok(None));
    }

    #[test]
    fn a_mount_point_needs_a_directory_of_stores() {
        assert_eq!(
            config(&[("CLADEFSD_MOUNT", "/m")]),
            Err("CLADEFSD_MOUNT is set, but CLADEFSD_MOUNT_STORES names no directory for its \
                 cells' stores"
                .to_owned())
        );
        assert_eq!(
            config(&[("CLADEFSD_MOUNT", "/m"), ("CLADEFSD_MOUNT_STORES", "/s")]),
            Ok(Some(Config { mountpoint: "/m".into(), stores: "/s".into(), allow_other: false }))
        );
        assert_eq!(
            config(&[
                ("CLADEFSD_MOUNT", "/m"),
                ("CLADEFSD_MOUNT_STORES", "/s"),
                ("CLADEFSD_MOUNT_ALLOW_OTHER", "1")
            ]),
            Ok(Some(Config { mountpoint: "/m".into(), stores: "/s".into(), allow_other: true }))
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
