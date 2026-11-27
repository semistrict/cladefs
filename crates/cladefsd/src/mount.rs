// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the daemon's FUSE mount of tenants' cells (TASK-475.2), the one mount of cells a host
//! has (`cladefs-mount` went, board 14305): one directory of cells, each Fxfs cell
//! attached shown at `TENANT/CELL` (`crate::fuse_control`), its Mount service
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
//! (doc-24): [`start`] refuses, and [`close`] has nothing to close; on macOS too, whose FUSE is
//! not Linux's (board 2912, board 13065).

use crate::Daemon;
#[cfg(all(target_os = "linux", not(madsim)))]
use crate::fuse_control::{self as control, CellSource};
use cladefs_core::error::Error;
#[cfg(all(target_os = "linux", not(madsim)))]
use cladefs_fs::fuse::{CellsMount, MountOptions};
use std::path::PathBuf;
#[cfg(all(target_os = "linux", not(madsim)))]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(all(target_os = "linux", not(madsim)))]
use std::sync::{Arc, Mutex};

/// Where the daemon mounts its cells, and keeps their cell directories.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The mount point, a directory: `CLADEFSD_MOUNT`.
    pub mountpoint: PathBuf,
    /// One or more JBOD directories the attached cell directories live in, each at `TENANT/CELL`:
    /// `CLADEFSD_MOUNT_STORES` (colon-separated).
    pub cell_dirs: Vec<PathBuf>,
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
        let raw_dirs = var("CLADEFSD_MOUNT_STORES").ok_or_else(|| {
            "CLADEFSD_MOUNT is set, but CLADEFSD_MOUNT_STORES names no directory for its \
             cell directories"
                .to_owned()
        })?;
        let cell_dirs: Vec<PathBuf> =
            std::env::split_paths(&raw_dirs).filter(|dir| !dir.as_os_str().is_empty()).collect();
        if cell_dirs.is_empty() {
            return Err("CLADEFSD_MOUNT is set, but CLADEFSD_MOUNT_STORES names no directory for \
                        its cell directories"
                .to_owned());
        }
        let allow_other = match var("CLADEFSD_MOUNT_ALLOW_OTHER").as_deref() {
            None | Some("0") => false,
            Some("1") => true,
            Some(other) => {
                return Err(format!("CLADEFSD_MOUNT_ALLOW_OTHER is 0 or 1, not {other}"));
            }
        };
        Ok(Some(Self { mountpoint: mountpoint.into(), cell_dirs, allow_other }))
    }
}

/// The service the mount's reader threads are, as the daemon's log names it when a panic on one
/// aborts the daemon (board 12443, hotbed question 561).
#[cfg(all(target_os = "linux", not(madsim)))]
const SERVICE: &str = "cladefsd fuse";

/// Per-daemon FUSE mount state, owned by [`Daemon`].
#[cfg(all(target_os = "linux", not(madsim)))]
#[derive(Default)]
pub(super) struct State {
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
#[cfg(all(target_os = "linux", not(madsim)))]
pub(super) fn start_on(daemon: &Daemon, config: &Config, bucket: String) -> Result<(), Error> {
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

/// Mounts the process's default daemon's directory of cells.
#[cfg(all(target_os = "linux", not(madsim)))]
pub fn start(config: &Config, bucket: String) -> Result<(), Error> {
    start_on(Daemon::process(), config, bucket)
}

/// The Mount service of `daemon`'s mount, if it has one ([`start_on`]). cladefs: none under madsim,
/// FUSE being outside the simulation (doc-24).
#[cfg(all(target_os = "linux", not(madsim)))]
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
/// simulation (doc-24); nor on macOS, whose FUSE is not Linux's (board 13065).
#[cfg(any(madsim, not(target_os = "linux")))]
pub(super) fn start_on(_daemon: &Daemon, config: &Config, _: String) -> Result<(), Error> {
    let why = match cfg!(madsim) {
        true => "FUSE is outside the simulation",
        false => "the daemon's FUSE mount is Linux's",
    };
    Err(Error::new(anyhow::anyhow!("mounting no cells at {}: {why}", config.mountpoint.display())))
}

/// cladefs: what `daemon`'s mount of cells holds and did, for its metrics (board 8565): its cells
/// attached, those being detached, and its server's counts; nothing, all 0, without a mount, or
/// once it closed, as under madsim, where a simulated host mounts none.
pub(super) fn counts_on(daemon: &Daemon) -> (usize, usize, cladefs_fs::fuse::Counts) {
    #[cfg(all(target_os = "linux", not(madsim)))]
    if let Some((mount, _)) = daemon.mount.mount.lock().unwrap().as_ref() {
        let server = mount.server();
        let attached = server.attached();
        let detaching = attached.iter().filter(|cell| cell.detaching).count();
        return (attached.len() - detaching, detaching, server.counts());
    }
    #[cfg(any(madsim, not(target_os = "linux")))]
    let _ = daemon;
    (0, 0, cladefs_fs::fuse::Counts::default())
}

/// cladefs: whether `daemon`'s FUSE mount currently has any cells attached or detaching: while
/// FUSE serves cells (`OpenCellDevice::read` bypasses `ExtentCache`, board 8260), the memory
/// governor scales `PartMemory` with remaining room so multi-file reads don't thrash zstd
/// decompression (board 12313).
pub(super) fn has_attached_on(daemon: &Daemon) -> bool {
    let (attached, detaching, _) = counts_on(daemon);
    attached + detaching > 0
}

/// Bytes of FUSE requests currently queued in per-tenant admission on `daemon` (board 9850).
pub(super) fn queued_bytes_on(daemon: &Daemon) -> u64 {
    #[cfg(all(target_os = "linux", not(madsim)))]
    return daemon.mount.queued_bytes.load(Ordering::Relaxed);
    #[cfg(any(madsim, not(target_os = "linux")))]
    {
        let _ = daemon;
        0
    }
}

/// Whether `daemon` mounted its cells and has not closed them, as [`close_on`] does, or failed to:
/// what a drain past its bound left.
pub(super) fn left_on(daemon: &Daemon) -> bool {
    #[cfg(all(target_os = "linux", not(madsim)))]
    return daemon.mount.mounted.load(Ordering::Acquire)
        && !daemon.mount.closed.load(Ordering::Acquire);
    #[cfg(any(madsim, not(target_os = "linux")))]
    {
        let _ = daemon;
        false
    }
}

/// Closes `daemon`'s mount, if it has one, as it drains: unmounted lazily, so at once from its
/// mount point, and once nothing uses it, its cells written back and closed. Holds
/// no thread of the runtime while it waits.
#[cfg(all(target_os = "linux", not(madsim)))]
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
#[cfg(any(madsim, not(target_os = "linux")))]
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
                cell_dirs: vec!["/s".into()],
                allow_other: false
            }))
        );
        assert_eq!(
            config(&[
                ("CLADEFSD_MOUNT", "/m"),
                ("CLADEFSD_MOUNT_STORES", "/s1:/s2"),
                ("CLADEFSD_MOUNT_ALLOW_OTHER", "1")
            ]),
            Ok(Some(Config {
                mountpoint: "/m".into(),
                cell_dirs: vec!["/s1".into(), "/s2".into()],
                allow_other: true
            }))
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
