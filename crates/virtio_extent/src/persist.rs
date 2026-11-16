// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The device in a snapshot of its VM (TASK-39.1), as plain values a VMM serializes in its own
//! snapshot format, beside its transport's state and [`VIRTIO_EXTENT_VERSION`], which a restore
//! checks first ([`check_version`]).
//!
//! The protocol (doc-4, Snapshots): the guest's filesystem is frozen, nothing open on the device
//! (`VIRTIO_EXTENT_T_FREEZE`), and the VM paused, its requests in flight served; the device forks
//! its store then, to the point the snapshot holds of it, and records the window's mappings
//! ([`Device::take_snapshot`](crate::Device::take_snapshot)). A VM restored from the snapshot opens
//! a store of its own, forked from that point if it doesn't exist (each restore its own, given
//! its path as it loads), and maps the window as it was, where it was in guest memory
//! ([`Device::restore`](crate::Device::restore)); its guest learns its store's branch as its
//! filesystem thaws.
//!
//! [`VIRTIO_EXTENT_VERSION`]: crate::protocol::VIRTIO_EXTENT_VERSION
//! [`check_version`]: crate::protocol::check_version

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::{CacheType, Config};

/// What a snapshot of its VM holds of an extent-store device, but its transport's state and its
/// protocol version. Firecracker's snapshots hold its fields in this order, after those two.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// Its configuration: its store's path is the one a restore opens, unless the snapshot's
    /// load overrides it.
    pub config: ConfigState,
    /// Where the window was in guest memory, if the device has one.
    pub window_addr: Option<u64>,
    /// The point the snapshot holds of the store: a fork of it.
    pub point: PathBuf,
    /// The point's branch, which a restore on another host takes from the extent daemon's bucket.
    pub point_branch: u64,
    /// The window's mappings.
    pub mappings: Vec<MappingState>,
    /// Whether the guest's filesystem was frozen: it is, as a snapshot is taken.
    pub frozen: bool,
}

/// A device's configuration ([`Config`]), as a snapshot holds it: a writer's, as a follower's
/// device takes no snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigState {
    pub id: String,
    pub tag: String,
    pub path_on_host: String,
    pub capacity: u64,
    pub dax_window_size: Option<u64>,
    /// cladefs: the window's private pages at most, as configured (doc-13): a count of pages, as
    /// snapshots before TASK-502 hold it, which configurations now give in bytes.
    pub dax_private_max: Option<u64>,
    pub extentd_socket: String,
    /// cladefs: what it does with the guest's asks for uploaded commits (TASK-363).
    pub cache_type: CacheType,
}

impl From<&Config> for ConfigState {
    fn from(config: &Config) -> Self {
        Self {
            id: config.id.clone(),
            tag: config.tag.clone(),
            path_on_host: config.path_on_host.clone(),
            capacity: config.capacity,
            dax_window_size: config.dax_window_size,
            dax_private_max: config
                .dax_private_max_bytes
                .map(|bytes| bytes / cladefs::pages::page_size()),
            extentd_socket: config.extentd_socket.clone(),
            cache_type: config.cache_type,
        }
    }
}

// cladefs: a follower's device takes no snapshot (`SnapshotError::ReadOnly`): a snapshot's is a
// writer's.
impl From<&ConfigState> for Config {
    fn from(config: &ConfigState) -> Self {
        Self {
            id: config.id.clone(),
            tag: config.tag.clone(),
            path_on_host: config.path_on_host.clone(),
            capacity: config.capacity,
            dax_window_size: config.dax_window_size,
            dax_private_max_bytes: config
                .dax_private_max
                .map(|pages| pages * cladefs::pages::page_size()),
            extentd_socket: config.extentd_socket.clone(),
            read_only: false,
            cache_type: config.cache_type,
        }
    }
}

/// A mapping of the window ([`cladefs::WindowMapping`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MappingState {
    pub window_offset: u64,
    pub len: u64,
    pub contents: MappingContentsState,
}

/// What a mapping of the window maps ([`cladefs::MappingContents`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MappingContentsState {
    Extent { branch: u64, sequence: u64, offset: u64 },
    Zeros,
}

impl From<&cladefs::WindowMapping> for MappingState {
    fn from(mapping: &cladefs::WindowMapping) -> Self {
        Self {
            window_offset: mapping.window_offset,
            len: mapping.len,
            contents: match mapping.contents {
                cladefs::MappingContents::Extent { extent, offset } => {
                    MappingContentsState::Extent {
                        branch: extent.branch,
                        sequence: extent.sequence,
                        offset,
                    }
                }
                cladefs::MappingContents::Zeros => MappingContentsState::Zeros,
            },
        }
    }
}

impl From<&MappingState> for cladefs::WindowMapping {
    fn from(mapping: &MappingState) -> Self {
        Self {
            window_offset: mapping.window_offset,
            len: mapping.len,
            contents: match mapping.contents {
                MappingContentsState::Extent { branch, sequence, offset } => {
                    cladefs::MappingContents::Extent {
                        extent: cladefs::ExtentId { branch, sequence },
                        offset,
                    }
                }
                MappingContentsState::Zeros => cladefs::MappingContents::Zeros,
            },
        }
    }
}

/// What a snapshot of the device holds of its store (TASK-39.1): the point it was taken at, a fork
/// of the store, which each VM restored from it forks again, and the window's mappings then, which
/// the restored VM's window maps again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub point: PathBuf,
    /// The point's branch: a restore on another host takes it from the extent daemon's bucket.
    pub point_branch: u64,
    pub mappings: Vec<cladefs::WindowMapping>,
}

impl From<&State> for Snapshot {
    fn from(state: &State) -> Self {
        Self {
            point: state.point.clone(),
            point_branch: state.point_branch,
            mappings: state.mappings.iter().map(cladefs::WindowMapping::from).collect(),
        }
    }
}

/// Errors taking a snapshot of a device (TASK-39.1).
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error(
        "Extent device {0}: its guest's filesystem is not frozen (fsfreeze it before the \
         snapshot, and thaw it after)"
    )]
    NotFrozen(String),
    #[error("Extent device {0}: it follows a store read-only, which a snapshot doesn't hold")]
    ReadOnly(String),
    #[error("Extent device {0}: cannot fork its store to {1}: {2}")]
    Fork(String, String, cladefs::Errno),
    #[error("Extent device {0}: cannot list its DAX window's mappings: {1}")]
    Mappings(String, cladefs::Errno),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A window's mappings, an extent's and zeros, come back from a snapshot as they went in.
    #[test]
    fn mappings_come_back_as_saved() {
        let mappings = [
            cladefs::WindowMapping {
                window_offset: 4096,
                len: 8192,
                contents: cladefs::MappingContents::Extent {
                    extent: cladefs::ExtentId { branch: 3, sequence: 9 },
                    offset: 12288,
                },
            },
            cladefs::WindowMapping {
                window_offset: 1 << 21,
                len: 4096,
                contents: cladefs::MappingContents::Zeros,
            },
        ];
        let saved: Vec<MappingState> = mappings.iter().map(MappingState::from).collect();
        assert_eq!(
            saved[0].contents,
            MappingContentsState::Extent { branch: 3, sequence: 9, offset: 12288 }
        );
        let restored: Vec<cladefs::WindowMapping> =
            saved.iter().map(cladefs::WindowMapping::from).collect();
        assert_eq!(restored, mappings);
    }

    /// A snapshot's configuration is a writer's, as it was configured. cladefs TASK-502: its
    /// private pages' cap a count of pages, as snapshots before held it, the configuration's bytes.
    #[test]
    fn configurations_come_back_a_writers() {
        let config = Config {
            id: "a".into(),
            tag: "fxfs".into(),
            path_on_host: "/store".into(),
            capacity: 1 << 30,
            dax_window_size: Some(64 << 20),
            dax_private_max_bytes: Some(16 * cladefs::pages::page_size()),
            extentd_socket: "/extentd.sock".into(),
            read_only: false,
            cache_type: CacheType::Unsafe,
        };
        assert_eq!(ConfigState::from(&config).dax_private_max, Some(16));
        assert_eq!(Config::from(&ConfigState::from(&config)), config);
    }
}
