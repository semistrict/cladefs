// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's extent store devices (doc-4): a guest's Fxfs, in its own kernel, keeps its
//! filesystem as extents of a cell, which cladefs's extent daemon serves, its file data mapped
//! into the guest through the device's DAX window from the daemon's extent cache, which the
//! host's VMs share (doc-30).
//!
//! libmachine is a library for one large process running many machines, with one extent daemon
//! that every machine of the process shares: an [`ExtentDaemon`], made once, which each
//! machine's [`MachineConfig::extent_daemon`](crate::MachineConfig::extent_daemon) names; each
//! machine's [`MachineConfig::extent_stores`](crate::MachineConfig::extent_stores) are its
//! devices, each opening a cell of its own on the daemon.
//!
//! The daemon is this process's own ([`ExtentDaemon::start`], libcladefs), its devices connected
//! to it in memory; or one listening at a socket ([`ExtentDaemon::at_socket`]), as a host's
//! `cladefsd` is.
//!
//! The devices are virtio-PCI devices of type 0x4658 with a request queue per vCPU and, given a
//! size, a DAX window: on KVM (x86-64 Linux) host memory cladefs maps with userfaultfd; on
//! Hypervisor.framework (arm64 macOS) guest memory libmachine maps as cladefs says (board 13069).

use std::path::PathBuf;

/// A cell of the daemon's, by tenant and ID.
pub use libcladefs::CellId;
/// What a daemon this process runs serves from ([`ExtentDaemon::start`]): its disks, bucket, KMS
/// and memory budget, as a host's `cladefsd`'s configuration says them.
pub use libcladefs::Config as ExtentDaemonConfig;

/// cladefs's extent daemon, as this process's machines reach it: made once, shared by every
/// machine with extent store devices ([`MachineConfig::extent_daemon`]), each of whose devices
/// connects to it as its machine is made.
///
/// [`MachineConfig::extent_daemon`]: crate::MachineConfig::extent_daemon
#[derive(Clone)]
pub struct ExtentDaemon {
    at: DaemonAt,
}

impl std::fmt::Debug for ExtentDaemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.at {
            DaemonAt::Embedded(_) => f.write_str("ExtentDaemon::Embedded"),
            DaemonAt::Socket(socket) => write!(f, "ExtentDaemon::Socket({})", socket.display()),
        }
    }
}

/// Where the daemon is.
#[derive(Clone)]
enum DaemonAt {
    /// In this process.
    Embedded(libcladefs::Daemon),
    /// Listening at a socket.
    Socket(PathBuf),
}

impl ExtentDaemon {
    /// Boots cladefs's extent daemon in this process, serving from `config`: the process's one
    /// (libcladefs), for its life. A process makes one at most: a second fails, as does one after
    /// [`ExtentDaemon::embedded`]'s.
    pub fn start(config: ExtentDaemonConfig) -> crate::Result<Self> {
        let daemon = futures::executor::block_on(libcladefs::Daemon::start(config))
            .map_err(|err| crate::Error::ExtentStore(format!("starting the daemon: {err:#}")))?;
        Ok(Self::embedded(daemon))
    }

    /// The daemon `daemon`, this process's, which its embedder booted itself (on its own runtime,
    /// serving its control API, as machined does).
    pub fn embedded(daemon: libcladefs::Daemon) -> Self {
        Self { at: DaemonAt::Embedded(daemon) }
    }

    /// The daemon listening at `socket`: a host's `cladefsd`, or one a test runs.
    pub fn at_socket(socket: impl Into<PathBuf>) -> Self {
        Self { at: DaemonAt::Socket(socket.into()) }
    }

    /// The daemon this process runs, if it is one ([`ExtentDaemon::start`]).
    pub fn daemon(&self) -> Option<&libcladefs::Daemon> {
        match &self.at {
            DaemonAt::Embedded(daemon) => Some(daemon),
            DaemonAt::Socket(_) => None,
        }
    }

    /// Imports the container image at `image` into a new cell of `tenant`'s, of `capacity` bytes, on
    /// the daemon this process runs ([`ExtentDaemon::start`]): a `docker export` tarball, or an OCI
    /// image layout and the `reference` of the image in it. A machine boots from it with an extent
    /// store device on it ([`ExtentCell::Existing`]) and `root=TAG rootfstype=cladefs rw` on its
    /// kernel's command line.
    pub fn import_image(
        &self,
        tenant: u64,
        capacity: u64,
        image: impl AsRef<std::path::Path>,
        reference: Option<&str>,
    ) -> crate::Result<CellId> {
        let daemon = self.daemon().ok_or_else(|| {
            crate::Error::InvalidConfig("only a daemon this process runs imports images".into())
        })?;
        futures::executor::block_on(daemon.import_image(tenant, capacity, image, reference))
            .map_err(|err| crate::Error::ExtentStore(format!("importing an image: {err:#}")))
    }

    /// A new connection to the daemon, for a device to open its cell over: the device's one seam
    /// to it.
    /// The socket the daemon listens at, if it isn't this process's.
    pub(crate) fn socket(&self) -> Option<&std::path::Path> {
        match &self.at {
            DaemonAt::Embedded(_) => None,
            DaemonAt::Socket(socket) => Some(socket),
        }
    }

    pub(crate) fn connect(&self) -> Result<cladefs::DaemonConnection, cladefs::Error> {
        match &self.at {
            DaemonAt::Embedded(daemon) => daemon.connect(),
            DaemonAt::Socket(socket) => cladefs::DaemonConnection::connect(socket),
        }
    }
}

/// What an extent store device does with the guest's asks for uploaded commits (its syncs, as a
/// block device's cache mode does with its flushes).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExtentCache {
    /// Each is answered once the extent daemon has uploaded the commit: a sync waits for the
    /// bucket.
    #[default]
    Writeback,
    /// Each is answered once ordered, the daemon uploading soon after: a sync waits for nothing
    /// past the host, and what a host's crash takes since its last upload is lost.
    Unsafe,
}

/// The cell an extent store device opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtentCell {
    /// A new cell of the tenant's, made on a disk of the daemon's as the machine is made.
    New {
        /// The tenant the cell is of.
        tenant: u64,
    },
    /// The daemon's existing cell.
    Existing(CellId),
    /// The cell in a directory on the host, made if it isn't there: a daemon without disks
    /// keeps its cells so.
    Directory(PathBuf),
}

impl ExtentCell {
    /// The cell `path` names as the daemon's open takes it, Firecracker's `path_on_host`: a
    /// cell's ID `TENANT/ID`, ID 0 making one of the tenant's; else a directory.
    pub fn from_path_on_host(path: &str) -> Self {
        match path.parse::<CellId>() {
            Ok(cell) if cell.id == 0 => Self::New { tenant: cell.tenant },
            Ok(cell) => Self::Existing(cell),
            Err(_) => Self::Directory(path.into()),
        }
    }

    /// The cell as the daemon's open names it: a directory's path, or a cell's ID, ID 0 making
    /// one.
    pub(crate) fn path_on_host(&self) -> Result<String, String> {
        match self {
            Self::New { tenant } => Ok(CellId { tenant: *tenant, id: 0 }.to_string()),
            Self::Existing(cell) if cell.id == 0 => Err(format!("cell {cell} names no cell")),
            Self::Existing(cell) => Ok(cell.to_string()),
            Self::Directory(dir) => (dir.to_str().map(str::to_owned))
                .ok_or_else(|| format!("its cell's path {} is not UTF-8", dir.display())),
        }
    }
}

impl From<PathBuf> for ExtentCell {
    fn from(dir: PathBuf) -> Self {
        Self::Directory(dir)
    }
}

impl From<CellId> for ExtentCell {
    fn from(cell: CellId) -> Self {
        Self::Existing(cell)
    }
}

/// An extent store device of a machine: a cell of cladefs's, which the guest mounts its Fxfs
/// from by the device's tag (`mount -t cladefs TAG DIR`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtentStore {
    /// The device's name in what libmachine says of it (errors, logs); unique in its machine.
    pub id: String,
    /// The filesystem's name, which the guest mounts it by: 1 to 36 bytes.
    pub tag: String,
    /// The cell the device opens.
    pub cell: ExtentCell,
    /// The most bytes a cell created holds.
    pub capacity: u64,
    /// The DAX window's size, a nonzero multiple of 2 MiB, through which the guest maps its files'
    /// data from the daemon's extent cache: none without one, every read and write then a
    /// request. Its guest physical addresses are a 64-bit BAR of the device's.
    pub dax_window_size: Option<u64>,
    /// The most bytes of private pages the window holds, as it copies on write (doc-13): pages
    /// the guest wrote and has not written back yet, this process's memory. A multiple of the
    /// page size, of the window's size at most; cladefs's default without it. A guest whose
    /// writes would pass it is stopped ([`Error::ExtentStore`](crate::Error::ExtentStore)).
    pub dax_private_max_bytes: Option<u64>,
    /// What the device does with the guest's syncs.
    pub cache: ExtentCache,
}

impl ExtentStore {
    /// A device of tag `tag` on `cell`, made with `capacity` bytes if it is new, its ID its tag,
    /// without a DAX window, its syncs waiting for the bucket.
    pub fn new(tag: impl Into<String>, cell: impl Into<ExtentCell>, capacity: u64) -> Self {
        let tag = tag.into();
        Self {
            id: tag.clone(),
            tag,
            cell: cell.into(),
            capacity,
            dax_window_size: None,
            dax_private_max_bytes: None,
            cache: ExtentCache::Writeback,
        }
    }
}
