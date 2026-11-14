// Copyright 2026 The fxdax Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A device's configuration, as a VMM's user gives it: in Firecracker's API, `PUT /extent/{id}`
//! and a VM's configuration file's `extent`, as this serializes.

use serde::{Deserialize, Serialize};

use crate::protocol::{
    DAX_WINDOW_ALIGNMENT, MAX_TAG_LEN, VIRTIO_EXTENT_F_DAX, VIRTIO_EXTENT_F_FREE_MANY,
    VIRTIO_EXTENT_F_FREEZE, VIRTIO_EXTENT_F_MAP_AHEAD, VIRTIO_EXTENT_F_MAP_MANY,
    VIRTIO_EXTENT_F_READ_ONLY, VIRTIO_EXTENT_F_RESIZE, VIRTIO_EXTENT_F_STATS,
    VIRTIO_EXTENT_F_WRITE_COMMIT,
};

/// fxdax: what the device does with the guest's asks for durability, as a block device's
/// `cache_type` does with its flushes (TASK-363). Firecracker's API names them as its block
/// devices' `CacheType` does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheType {
    /// Each is answered once ordered, as a block device's dropped flushes are: the daemon makes
    /// it durable soon after, without the guest waiting.
    Unsafe,
    /// Each is answered once durable, as the extent daemon makes it, uploaded.
    Writeback,
}

/// Errors in a device's configuration.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("The tag {0:?} is empty or longer than 36 bytes")]
    InvalidTag(String),
    #[error("The DAX window's size, {0:#x} bytes, is not a nonzero multiple of 2 MiB")]
    InvalidDaxWindowSize(u64),
    #[error(
        "The DAX window's private pages, {0}, are none, or more than it has pages, or it has no \
         window"
    )]
    InvalidDaxPrivateMax(u64),
}

/// An extent-store device: the store of extents that a guest running fxdax's Fxfs itself keeps
/// its filesystem in (fxdax's backlog doc-4), which fxdax's extent daemon serves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Unique identifier of the device.
    pub id: String,
    /// The filesystem's name, which the guest mounts it by.
    pub tag: String,
    /// The extent store's directory, created if it doesn't exist.
    pub path_on_host: String,
    /// The most bytes a store created holds.
    pub capacity: u64,
    /// The size in bytes of the DAX window, the device's shared memory region the guest maps
    /// files' data from: none without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dax_window_size: Option<u64>,
    /// fxdax: the most private pages a writer's window holds, as it copies on write (doc-4's
    /// `DAX`, fxdax's backlog doc-13), which fxdax's extent daemon reserves against the store's
    /// tenant's memory as the guest activates the device: 1 GiB's pages without it, or the
    /// window's if fewer, whatever its size (TASK-227). A follower's window copies nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dax_private_max: Option<u64>,
    /// The socket of fxdax's extent daemon, which serves the store.
    pub extentd_socket: String,
    /// fxdax: whether the device follows the store, read-only, as another VM writes it (fxdax's
    /// backlog doc-8): the store must exist, and the guest mounts it read-only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    /// fxdax: what the device does with the guest's asks for durability, as a block device's
    /// `cache_type` does with its flushes (TASK-363): `Writeback`, the default, waits for each, the
    /// store's commits durable as the extent daemon makes them, uploaded; `Unsafe` answers them once
    /// ordered, as a block device's dropped flushes are, the daemon making them durable soon after,
    /// without the guest waiting: sync, syncfs, a freeze and an unmount wait for nothing past the
    /// host, and what a host's crash takes since its last upload is lost, as it is of a disk image
    /// whose flushes the host drops.
    #[serde(default = "writeback", skip_serializing_if = "is_writeback")]
    pub cache_type: CacheType,
}

fn writeback() -> CacheType {
    CacheType::Writeback
}

fn is_writeback(cache: &CacheType) -> bool {
    *cache == CacheType::Writeback
}

impl Config {
    /// `Ok` if the configuration describes a device: a tag of 1 to 36 bytes; a window, if any, a
    /// nonzero multiple of 2 MiB; and some of its pages private at most, if said, of a window.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.tag.is_empty() || self.tag.len() > MAX_TAG_LEN {
            return Err(ConfigError::InvalidTag(self.tag.clone()));
        }
        if let Some(size) = self.dax_window_size
            && (size == 0 || !size.is_multiple_of(DAX_WINDOW_ALIGNMENT))
        {
            return Err(ConfigError::InvalidDaxWindowSize(size));
        }
        // fxdax: some of the window's pages, of a window (doc-13).
        if let Some(most) = self.dax_private_max {
            let pages = self.dax_window_size.unwrap_or(0) / fxdax::pages::page_size();
            if most == 0 || most > pages {
                return Err(ConfigError::InvalidDaxPrivateMax(most));
            }
        }
        Ok(())
    }

    /// The device's own features, as configured; its transport's (`VIRTIO_F_VERSION_1`,
    /// `VIRTIO_RING_F_EVENT_IDX`) are the VMM's to add. A follower's device writes nothing
    /// (doc-4's `F_READ_ONLY`); the daemon asks the guest for its stats, a follower's as a
    /// writer's (TASK-111.4); a window's MAP may carry a chunk's pieces (`MAP_MANY`, TASK-91.4) and
    /// say the guest reads them next (`MAP_AHEAD`, TASK-112.1).
    pub fn features(&self) -> u64 {
        let mut features = if self.read_only {
            (1 << VIRTIO_EXTENT_F_FREEZE)
                | (1 << VIRTIO_EXTENT_F_READ_ONLY)
                | (1 << VIRTIO_EXTENT_F_STATS)
        } else {
            (1 << VIRTIO_EXTENT_F_WRITE_COMMIT)
                | (1 << VIRTIO_EXTENT_F_RESIZE)
                | (1 << VIRTIO_EXTENT_F_FREEZE)
                | (1 << VIRTIO_EXTENT_F_FREE_MANY)
                | (1 << VIRTIO_EXTENT_F_STATS)
        };
        if self.dax_window_size.is_some() {
            features |= (1 << VIRTIO_EXTENT_F_DAX)
                | (1 << VIRTIO_EXTENT_F_MAP_MANY)
                | (1 << VIRTIO_EXTENT_F_MAP_AHEAD);
        }
        features
    }
}

/// fxdax: the most private pages a writer's window of `len` bytes holds as it copies on write
/// (backlog doc-13): `configured`, if given; otherwise [`PRIVATE_MAX_DEFAULT`]'s pages, or the
/// window's if fewer, one at least (TASK-227; half its pages before, TASK-164.11).
pub fn private_max(len: u64, configured: Option<u64>) -> u64 {
    let pages = len / fxdax::pages::page_size();
    configured
        .unwrap_or_else(|| (PRIVATE_MAX_DEFAULT / fxdax::pages::page_size()).min(pages))
        .max(1)
}

/// fxdax: the bytes of private pages a writer's window holds at most unless configured
/// (`dax_private_max`), whatever its size (TASK-227): enough for a forked pgbench's working set
/// (GCE r804, forked-dax 0.91), which the daemon reserves against the tenant's memory.
pub const PRIVATE_MAX_DEFAULT: u64 = 1 << 30;

#[cfg(test)]
mod tests {
    use super::*;

    fn config(id: &str) -> Config {
        Config {
            id: id.into(),
            tag: "fxfs".into(),
            path_on_host: "/store".into(),
            capacity: 1 << 30,
            dax_window_size: Some(64 << 20),
            dax_private_max: None,
            extentd_socket: "/extentd.sock".into(),
            read_only: false,
            cache_type: CacheType::Writeback,
        }
    }

    /// fxdax: a device's cache is Writeback unless its configuration says Unsafe (TASK-363).
    #[test]
    fn caches_are_writeback_unless_unsafe() {
        let json =
            r#"{"id":"a","tag":"fxfs","path_on_host":"/s","capacity":1,"extentd_socket":"/e"}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.cache_type, CacheType::Writeback);
        assert_eq!(serde_json::to_string(&config).unwrap(), json);
        let unsafe_json = json.replace('}', r#","cache_type":"Unsafe"}"#);
        let config: Config = serde_json::from_str(&unsafe_json).unwrap();
        assert_eq!(config.cache_type, CacheType::Unsafe);
        assert_eq!(serde_json::to_string(&config).unwrap(), unsafe_json);
    }

    #[test]
    fn configurations_deserialize() {
        let json = r#"{
            "id": "extents",
            "tag": "fxfs",
            "path_on_host": "/store",
            "capacity": 1073741824,
            "dax_window_size": 67108864,
            "extentd_socket": "/extentd.sock"
        }"#;
        let parsed: Config = serde_json::from_str(json).unwrap();
        assert_eq!(parsed, config("extents"));
        let read_only = json.replace('}', r#", "read_only": true }"#);
        let parsed: Config = serde_json::from_str(&read_only).unwrap();
        assert_eq!(parsed, Config { read_only: true, ..config("extents") });
        let unknown = json.replace('}', r#", "dax": true }"#);
        assert!(serde_json::from_str::<Config>(&unknown).is_err());
    }

    #[test]
    fn invalid_configurations_are_refused() {
        config("a").validate().unwrap();
        let long = Config { tag: "x".repeat(37), ..config("a") };
        assert_eq!(long.validate(), Err(ConfigError::InvalidTag("x".repeat(37))));
        let empty = Config { tag: String::new(), ..config("a") };
        assert_eq!(empty.validate(), Err(ConfigError::InvalidTag(String::new())));
        let unaligned = Config { dax_window_size: Some(3 << 20 >> 1), ..config("a") };
        assert_eq!(unaligned.validate(), Err(ConfigError::InvalidDaxWindowSize(3 << 20 >> 1)));
        assert_eq!(
            Config { dax_window_size: Some(0), ..config("a") }.validate(),
            Err(ConfigError::InvalidDaxWindowSize(0))
        );
        // fxdax: some of the window's pages private at most, of a window.
        let pages = (64 << 20) / fxdax::pages::page_size();
        for most in [0, pages + 1] {
            let private = Config { dax_private_max: Some(most), ..config("a") };
            assert_eq!(private.validate(), Err(ConfigError::InvalidDaxPrivateMax(most)));
        }
        Config { dax_private_max: Some(pages), ..config("a") }.validate().unwrap();
        let none = Config { dax_window_size: None, ..config("a") };
        none.validate().unwrap();
        let private = Config { dax_private_max: Some(1), ..none };
        assert_eq!(private.validate(), Err(ConfigError::InvalidDaxPrivateMax(1)));
    }

    /// A writer's features and a follower's, with a window and without.
    #[test]
    fn features_are_as_configured() {
        let writer = config("a");
        assert_eq!(writer.features(), 0b1001_1101_1101);
        let follower = Config { read_only: true, ..config("a") };
        assert_eq!(follower.features(), 0b1001_1011_0001);
        assert_eq!(Config { dax_window_size: None, ..writer }.features(), 0b1000_0101_1100);
        assert_eq!(Config { dax_window_size: None, ..follower }.features(), 0b1000_0011_0000);
    }

    /// fxdax TASK-227: a writer's window holds 1 GiB of private pages at most unless configured
    /// otherwise, whatever its size: the window is address space, as large as the data, and its
    /// private pages are memory, which the daemon reserves against the tenant's. Half a 64 GiB
    /// window's pages, as TASK-164.11's default had it, would have reserved 32 GiB a VM. A window
    /// smaller than the default holds as many as it has pages; one of a page, one.
    #[test]
    fn a_windows_private_pages_are_not_its_size() {
        let page = fxdax::pages::page_size();
        let gib = (1 << 30) / page;
        assert_eq!(private_max(64 << 30, None), gib);
        assert_eq!(private_max(2 << 30, None), gib);
        assert_eq!(private_max(512 << 20, None), (512 << 20) / page);
        assert_eq!(private_max(64 << 30, Some(16)), 16);
        assert_eq!(private_max(page, None), 1);
    }
}
