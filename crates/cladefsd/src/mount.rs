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
//! ([`Config::from_env`]), and closed as it drains (`close_on`): unmounted lazily, its
//! cells written back and closed once nothing uses them, within the drain's
//! bound. Unmounted otherwise meanwhile (`umount`), it serves no more, and the daemon goes on
//! serving VMs; its cells close as it drains.
//!
//! cladefs: a daemon mounts cells if it is built to (the `mount` feature, on unless a build says
//! otherwise, board 14536), on Linux, outside the simulation: `mount/fuse.rs`, over `cladefs_fuse`.
//! Any other mounts none (`mount/none.rs`), and says why to whatever asks it to: built without the
//! feature, it links none of `cladefs_fuse`; under madsim, FUSE is outside the simulation
//! (doc-24); and macOS's FUSE is not Linux's (board 2912, board 13065).

use crate::Daemon;
use std::path::PathBuf;

#[cfg_attr(all(feature = "mount", target_os = "linux", not(madsim)), path = "mount/fuse.rs")]
#[cfg_attr(not(all(feature = "mount", target_os = "linux", not(madsim))), path = "mount/none.rs")]
mod built;
pub(super) use built::*;

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

/// Mounts the process's default daemon's directory of cells. cladefs: outside the simulation, as
/// [`Daemon::process`] is.
#[cfg(not(madsim))]
pub fn start(config: &Config, bucket: String) -> Result<(), cladefs_core::error::Error> {
    start_on(Daemon::process(), config, bucket)
}

/// cladefs: whether `daemon`'s FUSE mount currently has any cells attached or detaching: while
/// FUSE serves cells (`OpenCellDevice::read` bypasses `ExtentCache`, board 8260), the memory
/// governor scales `PartMemory` with remaining room so multi-file reads don't thrash zstd
/// decompression (board 12313).
pub(super) fn has_attached_on(daemon: &Daemon) -> bool {
    let (attached, detaching, _) = counts_on(daemon);
    attached + detaching > 0
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
