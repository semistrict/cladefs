// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A device's configuration, as a VMM's user gives it: in Firecracker's API, `PUT /cladefs/{id}`
//! and a VM's configuration file's `cladefs`, as this serializes.

use serde::{Deserialize, Serialize};

use crate::protocol::{
    DAX_WINDOW_ALIGNMENT, MAX_TAG_LEN, VIRTIO_EXTENT_F_DAX, VIRTIO_EXTENT_F_FREE_MANY,
    VIRTIO_EXTENT_F_FREEZE, VIRTIO_EXTENT_F_MAP_MANY, VIRTIO_EXTENT_F_MAP_PREFAULT,
    VIRTIO_EXTENT_F_RESIZE, VIRTIO_EXTENT_F_STATS, VIRTIO_EXTENT_F_STREAM,
    VIRTIO_EXTENT_F_WRITE_COMMIT,
};

/// cladefs: what the device does with the guest's asks for uploaded commits, as a block device's
/// `cache_type` does with its flushes (TASK-363). Firecracker's API names them as its block
/// devices' `CacheType` does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheType {
    /// Each is answered once ordered, as a block device's dropped flushes are: the daemon makes
    /// it local soon after, and uploaded, without the guest waiting.
    Unsafe,
    /// Each is answered once uploaded, as the extent daemon makes it.
    Writeback,
}

impl CacheType {
    /// Whether the guest's uploaded commits are answered once ordered: the guest's dialect's
    /// (`cladefs::extent_requests::Guest::ordered_only`).
    pub fn ordered_only(self) -> bool {
        self == CacheType::Unsafe
    }
}

/// Errors in a device's configuration.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("The tag {0:?} is empty or longer than 36 bytes")]
    InvalidTag(String),
    #[error("The DAX window's size, {0:#x} bytes, is not a nonzero multiple of 2 MiB")]
    InvalidDaxWindowSize(u64),
    #[error(
        "The DAX window's private pages' bytes, {0:#x}, are none, not a multiple of the page size, \
         or more than the window's, or it has no window"
    )]
    InvalidDaxPrivateMaxBytes(u64),
    #[error("The stream's port, {0}, is not a vsock port a guest connects to (1 to 0xfffffffe)")]
    InvalidStreamPort(u32),
}

/// cladefs: where the guest's data requests go over vsock to the extent daemon, which serves them
/// itself (doc-4's stream transport, `F_STREAM`, board 14663): the host's vsock `port` the guest
/// connects to, and the path its VMM's vsock device connects such a guest's connections to,
/// `listener`, where the device binds a socket the daemon accepts them on (Firecracker's
/// `<uds_path>_<port>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamConfig {
    pub port: u32,
    pub listener: String,
}

/// An extent-store device: the cell of extents that a guest running cladefs's Fxfs itself keeps
/// its filesystem in (cladefs's backlog doc-4), which cladefs's extent daemon serves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Unique identifier of the device.
    pub id: String,
    /// The filesystem's name, which the guest mounts it by.
    pub tag: String,
    /// The cell directory, created if it doesn't exist.
    pub path_on_host: String,
    /// The most bytes a cell created holds.
    pub capacity: u64,
    /// The size in bytes of the DAX window, the device's shared memory region the guest maps
    /// files' data from: none without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dax_window_size: Option<u64>,
    /// cladefs: the most bytes of private pages the window holds, as it copies on write
    /// (doc-4's `DAX`, cladefs's backlog doc-13): pages the guest wrote and has not written back
    /// yet, the VM process's memory, for which cladefs's extent daemon reserves nothing
    /// (TASK-501). A multiple of the page size, a page at least, the window's size at most;
    /// [`PRIVATE_MAX_DEFAULT`] without it, or the window's if less, whatever its size (TASK-227).
    /// The VM manager sizes it per VM (TASK-502); a page count was `dax_private_max`, which is
    /// refused now, not read as bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dax_private_max_bytes: Option<u64>,
    /// The socket of cladefs's extent daemon, which serves the cell.
    pub cladefsd_socket: String,
    /// cladefs: what the device does with the guest's asks for uploaded commits, as a block
    /// device's `cache_type` does with its flushes (TASK-363): `Writeback`, the default, waits for
    /// each, the cell's commits uploaded as the extent daemon makes them; `Unsafe` answers them
    /// once ordered, as a block device's dropped flushes are, the daemon making them local soon
    /// after, without the guest waiting: sync, syncfs, a freeze and an unmount wait for nothing
    /// past the host, and what a host's crash takes since its last upload is lost, as it is of a
    /// disk image whose flushes the host drops.
    #[serde(default = "writeback", skip_serializing_if = "is_writeback")]
    pub cache_type: CacheType,
    /// cladefs: the guest's data requests go over vsock to the extent daemon, rather than to the
    /// device's queues ([`StreamConfig`], board 14663). Its VM has a vsock device, whose
    /// connections to `port` reach `listener`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<StreamConfig>,
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
        // cladefs: some of the window's pages, whole, of a window (doc-13, TASK-502).
        if let Some(most) = self.dax_private_max_bytes {
            let window = self.dax_window_size.unwrap_or(0);
            if most == 0 || !most.is_multiple_of(cladefs::pages::page_size()) || most > window {
                return Err(ConfigError::InvalidDaxPrivateMaxBytes(most));
            }
        }
        // cladefs: a port a guest's connect names, not VMADDR_PORT_ANY (board 14663).
        if let Some(stream) = &self.stream
            && (stream.port == 0 || stream.port == u32::MAX)
        {
            return Err(ConfigError::InvalidStreamPort(stream.port));
        }
        Ok(())
    }

    /// The device's own features, as configured; its transport's (`VIRTIO_F_VERSION_1`,
    /// `VIRTIO_RING_F_EVENT_IDX`) are the VMM's to add. The daemon asks the guest for its stats
    /// (TASK-111.4); a window's MAP may carry a chunk's pieces (`MAP_MANY`, TASK-91.4) and say the
    /// guest reads them next (`MAP_PREFAULT`, TASK-112.1).
    pub fn features(&self) -> u64 {
        let mut features = (1 << VIRTIO_EXTENT_F_WRITE_COMMIT)
            | (1 << VIRTIO_EXTENT_F_RESIZE)
            | (1 << VIRTIO_EXTENT_F_FREEZE)
            | (1 << VIRTIO_EXTENT_F_FREE_MANY)
            | (1 << VIRTIO_EXTENT_F_STATS);
        if self.dax_window_size.is_some() {
            features |= (1 << VIRTIO_EXTENT_F_DAX)
                | (1 << VIRTIO_EXTENT_F_MAP_MANY)
                | (1 << VIRTIO_EXTENT_F_MAP_PREFAULT);
        }
        if self.stream.is_some() {
            features |= 1 << VIRTIO_EXTENT_F_STREAM;
        }
        features
    }
}

/// cladefs: the most private pages a window of `len` bytes holds as it copies on write
/// (backlog doc-13), as `WINDOW` names them: `configured` bytes' pages, if given; otherwise
/// [`PRIVATE_MAX_DEFAULT`]'s, or the window's if fewer, one at least (TASK-227; half its pages
/// before, TASK-164.11).
pub fn private_max(len: u64, configured: Option<u64>) -> u64 {
    let bytes = configured.unwrap_or(PRIVATE_MAX_DEFAULT.min(len));
    (bytes / cladefs::pages::page_size()).max(1)
}

/// cladefs: the bytes of private pages a window holds at most unless configured
/// (`dax_private_max_bytes`), whatever its size (TASK-227): enough for a forked pgbench's working
/// set at 2 vCPUs (GCE r804, forked-dax 0.91). The VM manager sizes it per VM (TASK-502).
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
            dax_private_max_bytes: None,
            cladefsd_socket: "/cladefsd.sock".into(),
            cache_type: CacheType::Writeback,
            stream: None,
        }
    }

    #[test]
    fn configurations_deserialize() {
        let json = r#"{
            "id": "extents",
            "tag": "fxfs",
            "path_on_host": "/store",
            "capacity": 1073741824,
            "dax_window_size": 67108864,
            "cladefsd_socket": "/cladefsd.sock"
        }"#;
        let parsed: Config = serde_json::from_str(json).unwrap();
        assert_eq!(parsed, config("extents"));
        // cladefs: a follower's, which devices are no more (board 8237), refused.
        let read_only = json.replace('}', r#", "read_only": true }"#);
        assert!(serde_json::from_str::<Config>(&read_only).is_err());
        let unknown = json.replace('}', r#", "dax": true }"#);
        assert!(serde_json::from_str::<Config>(&unknown).is_err());
        // cladefs TASK-502: the cap in bytes; its page count, as it was, refused, not read as
        // bytes.
        let bytes = json.replace('}', r#", "dax_private_max_bytes": 1048576 }"#);
        let parsed: Config = serde_json::from_str(&bytes).unwrap();
        assert_eq!(parsed.dax_private_max_bytes, Some(1 << 20));
        let pages = json.replace('}', r#", "dax_private_max": 256 }"#);
        assert!(serde_json::from_str::<Config>(&pages).is_err());
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
        // cladefs TASK-502: some of the window's pages' bytes private at most, whole pages, of a
        // window.
        let (page, window) = (cladefs::pages::page_size(), 64 << 20);
        for most in [0, page - 1, page + 1, window + page] {
            let private = Config { dax_private_max_bytes: Some(most), ..config("a") };
            assert_eq!(private.validate(), Err(ConfigError::InvalidDaxPrivateMaxBytes(most)));
        }
        for most in [page, window] {
            Config { dax_private_max_bytes: Some(most), ..config("a") }.validate().unwrap();
        }
        let none = Config { dax_window_size: None, ..config("a") };
        none.validate().unwrap();
        let private = Config { dax_private_max_bytes: Some(page), ..none };
        assert_eq!(private.validate(), Err(ConfigError::InvalidDaxPrivateMaxBytes(page)));
        // cladefs board 14663: a stream to a port a guest can connect to.
        let stream = |port| Config {
            stream: Some(StreamConfig { port, listener: "/vsock.sock_5000".into() }),
            ..config("a")
        };
        stream(5000).validate().unwrap();
        for port in [0, u32::MAX] {
            assert_eq!(stream(port).validate(), Err(ConfigError::InvalidStreamPort(port)));
        }
        assert_eq!(
            stream(5000).features() & (1 << VIRTIO_EXTENT_F_STREAM),
            1 << VIRTIO_EXTENT_F_STREAM
        );
        assert_eq!(config("a").features() & (1 << VIRTIO_EXTENT_F_STREAM), 0);
    }

    /// cladefs TASK-227: a writer's window holds 1 GiB of private pages at most unless configured
    /// otherwise, whatever its size: the window is address space, as large as the data, and its
    /// private pages are memory, the VM process's. Half a 64 GiB window's pages, as TASK-164.11's
    /// default had it, would have let a VM hold 32 GiB of them. A window smaller than the default
    /// holds as many as it has pages; one of a page, one. Configured, its bytes are as many pages
    /// (TASK-502).
    #[test]
    fn a_windows_private_pages_are_not_its_size() {
        let page = cladefs::pages::page_size();
        let gib = (1 << 30) / page;
        assert_eq!(private_max(64 << 30, None), gib);
        assert_eq!(private_max(2 << 30, None), gib);
        assert_eq!(private_max(512 << 20, None), (512 << 20) / page);
        assert_eq!(private_max(64 << 30, Some(16 * page)), 16);
        assert_eq!(private_max(64 << 30, Some(4 << 30)), 4 * gib);
        assert_eq!(private_max(page, None), 1);
    }
}
