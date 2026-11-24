// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The device in a snapshot of its VM (TASK-39.1), as plain values a VMM serializes in its own
//! snapshot format, beside its transport's state and [`VIRTIO_EXTENT_VERSION`], which a restore
//! checks first ([`check_version`]).
//!
//! The protocol (doc-4, Snapshots): the guest's filesystem is frozen, nothing open on the device
//! (`VIRTIO_EXTENT_T_FREEZE`), and the VM paused, its requests in flight served; the device forks
//! its cell then, to the version the snapshot holds of it, and records the window's mappings
//! ([`Device::take_snapshot`](crate::Device::take_snapshot)). A VM restored from the snapshot opens
//! a cell of its own, forked from that version if it doesn't exist (each restore its own, given
//! its path as it loads), and maps the window as it was, where it was in guest memory
//! ([`Device::restore`](crate::Device::restore)); its guest learns its cell ID as its
//! filesystem thaws.
//!
//! [`VIRTIO_EXTENT_VERSION`]: crate::protocol::VIRTIO_EXTENT_VERSION
//! [`check_version`]: crate::protocol::check_version

use serde::{Deserialize, Serialize};

use crate::config::{CacheType, Config};

/// What a snapshot of its VM holds of an extent-store device, but its transport's state and its
/// protocol version. Firecracker's snapshots hold its fields in this order, after those two.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// Its configuration: its cell's path is the one a restore opens, unless the snapshot's
    /// load overrides it.
    pub config: ConfigState,
    /// Where the window was in guest memory, if the device has one.
    pub window_addr: Option<u64>,
    /// The version of the cell the snapshot holds, which its restores fork, on any host.
    pub version: cladefs::CellVersion,
    /// The window's mappings.
    pub mappings: Vec<MappingState>,
    /// Whether the guest's filesystem was frozen: it is, as a snapshot is taken.
    pub frozen: bool,
}

/// A device's configuration ([`Config`]), as a snapshot holds it.
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
    pub cladefsd_socket: String,
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
            cladefsd_socket: config.cladefsd_socket.clone(),
            cache_type: config.cache_type,
        }
    }
}

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
            cladefsd_socket: config.cladefsd_socket.clone(),
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
    Extent { cell: u64, sequence: u64, offset: u64 },
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
                        cell: extent.cell,
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
                MappingContentsState::Extent { cell, sequence, offset } => {
                    cladefs::MappingContents::Extent {
                        extent: cladefs::ExtentId { cell, sequence },
                        offset,
                    }
                }
                MappingContentsState::Zeros => cladefs::MappingContents::Zeros,
            },
        }
    }
}

/// What a snapshot of the device holds of its cell (TASK-39.1): the version of its cell it was
/// taken at, which each VM restored from it forks, and the window's mappings then, which the
/// restored VM's window maps again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub version: cladefs::CellVersion,
    pub mappings: Vec<cladefs::WindowMapping>,
}

impl From<&State> for Snapshot {
    fn from(state: &State) -> Self {
        Self {
            version: state.version,
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
    #[error("Extent device {0}: cannot make a version of its cell: {1}")]
    Version(String, cladefs::Errno),
    #[error("Extent device {0}: cannot list its DAX window's mappings: {1}")]
    Mappings(String, cladefs::Errno),
    #[error(
        "Extent device {0}: its DAX window holds {1} private pages, whose bytes no extent holds \
         (freeze the guest's filesystem, which rebinds them)"
    )]
    Private(String, u64),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A snapshot's configuration comes back as it was configured. cladefs TASK-502: its private
    /// pages' cap a count of pages, as snapshots before held it, the configuration's bytes.
    #[test]
    fn configurations_come_back_as_configured() {
        let config = Config {
            id: "a".into(),
            tag: "fxfs".into(),
            path_on_host: "/store".into(),
            capacity: 1 << 30,
            dax_window_size: Some(64 << 20),
            dax_private_max_bytes: Some(16 * cladefs::pages::page_size()),
            cladefsd_socket: "/cladefsd.sock".into(),
            cache_type: CacheType::Unsafe,
        };
        assert_eq!(ConfigState::from(&config).dax_private_max, Some(16));
        assert_eq!(Config::from(&ConfigState::from(&config)), config);
    }
}
