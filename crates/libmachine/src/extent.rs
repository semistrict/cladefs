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
//! The device's connection to the daemon has one seam (`ExtentDaemon::connect`): until
//! libcladefs (board 13471) runs the daemon embedded in this process, connected to in memory,
//! it is a daemon listening at a socket, as a host's or a test's is; a macOS helper process
//! (board 13460) reaches its server's over the same wire.
//!
//! On KVM (x86-64 Linux) the devices are virtio-PCI devices of type 0x4658 with a request queue
//! per vCPU and, given a size, a DAX window. On Hypervisor.framework, none yet: virtio_extent's
//! device and cladefs's daemon client build on Linux alone until the daemon comes to macOS
//! (board 13471), and the window then without DAX first (board 13067, board 13069).

use std::path::PathBuf;

/// cladefs's extent daemon, as this process's machines reach it: made once, shared by every
/// machine with extent store devices ([`MachineConfig::extent_daemon`]), each of whose devices
/// connects to it as its machine is made.
///
/// [`MachineConfig::extent_daemon`]: crate::MachineConfig::extent_daemon
#[derive(Debug)]
pub struct ExtentDaemon {
    at: DaemonAt,
}

/// Where the daemon is.
#[derive(Debug)]
enum DaemonAt {
    /// Listening at a socket.
    Socket(PathBuf),
}

impl ExtentDaemon {
    /// The daemon listening at `socket`: a host's `cladefsd`, or one a test runs in its own
    /// process. Until libcladefs (board 13471) lets libmachine run the daemon itself, embedded,
    /// as this process's own, with its storage and memory budget from here.
    pub fn at_socket(socket: impl Into<PathBuf>) -> Self {
        Self { at: DaemonAt::Socket(socket.into()) }
    }

    /// A new connection to the daemon, for a device to open its cell over: the device's one seam
    /// to it.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    pub(crate) fn connect(&self) -> Result<cladefs::DaemonConnection, cladefs::Error> {
        match &self.at {
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

/// An extent store device of a machine: a cell of cladefs's, which the guest mounts its Fxfs
/// from by the device's tag (`mount -t cladefs TAG DIR`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtentStore {
    /// The device's name in what libmachine says of it (errors, logs); unique in its machine.
    pub id: String,
    /// The filesystem's name, which the guest mounts it by: 1 to 36 bytes.
    pub tag: String,
    /// The cell directory on the host, created if it doesn't exist.
    pub cell: PathBuf,
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
    /// A device of tag `tag` on the cell at `cell`, created with `capacity` bytes if it isn't
    /// there, its ID its tag, without a DAX window, its syncs waiting for the bucket.
    pub fn new(tag: impl Into<String>, cell: impl Into<PathBuf>, capacity: u64) -> Self {
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
