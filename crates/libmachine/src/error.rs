// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/// Errors returned by libmachine.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The machine configuration is invalid.
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    /// The kernel or initrd cannot be booted (unknown format, no entry point,
    /// does not fit in guest RAM, ...).
    #[error("cannot boot: {0}")]
    Boot(String),

    /// Reading a file the machine needs failed.
    #[error("{}: {source}", path.display())]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The hypervisor rejected an operation.
    #[error("hypervisor error during {op}: {source}")]
    Hypervisor {
        op: &'static str,
        #[source]
        source: std::io::Error,
    },

    /// A guest memory access failed (e.g. the address is outside guest RAM).
    #[error("guest memory error: {0}")]
    GuestMemory(#[from] vm_memory::GuestMemoryError),

    /// Memory cannot be mapped into the guest there (unaligned, outside the
    /// MMIO windows, overlapping other device memory, no memory slot free).
    #[error("cannot map memory into the guest: {0}")]
    MemoryMap(String),

    /// Allocating or mapping guest memory failed.
    #[error("failed to allocate guest memory: {0}")]
    MemoryAllocation(String),

    /// The guest agent could not be reached (it did not answer within the
    /// retry budget) or broke the protocol (another version, a malformed
    /// answer).
    #[error("guest agent: {0}")]
    Agent(String),

    /// The guest is not running any more: it powered off, reset or
    /// triple-faulted (or its machine was dropped), so nothing in it will
    /// answer. A command whose request went out may have run.
    #[error("the guest is gone: {0}")]
    GuestGone(String),

    /// An operation in the guest failed: `source` holds the guest's errno
    /// (Linux numbering, as on the host).
    #[error("in the guest: {what}")]
    Guest {
        what: String,
        #[source]
        source: std::io::Error,
    },

    /// The connection to the guest agent was reset, and a command could not
    /// be taken up again: the agent restarted (so the command may or may
    /// not have run), forgot it, or no longer keeps output this side had
    /// not received. It is not run again.
    #[error("connection to the guest agent reset: {0}")]
    ConnectionReset(String),

    /// Waiting for the guest took longer than allowed.
    #[error("timed out: {0}")]
    TimedOut(String),

    /// A machine cannot be snapshotted (a device that cannot save its
    /// state, a network), or a snapshot cannot be restored: another format
    /// version, a damaged file, or a machine that differs from the one it
    /// was taken of (CPU model, vCPU count, address map). Snapshots are
    /// never converted.
    #[error("snapshot: {0}")]
    Snapshot(String),

    /// An extent store device stopped the machine for good: it could not be activated, or failed
    /// otherwise than by its daemon's going ([`Error::ExtentDaemonLost`]) or a guest's access
    /// refused ([`Error::AccessRefused`]). Every run of the machine ends at once with it from then
    /// on.
    #[error("extent store device: {0}")]
    ExtentStore(String),

    /// An extent store device's extent daemon went, and the guest's disk with it: the machine
    /// stopped for good, or wasn't made, the daemon going as its cell opened (Firecracker's
    /// `CladefsdLost`, which a VMM tells its host apart from other failures). Every run of the
    /// machine ends at once with it from then on.
    #[error("extent store device: {0}")]
    ExtentDaemonLost(String),

    /// cladefs: a guest's access to an extent store device's DAX window was refused for good, and
    /// the machine can't go on: a page nothing maps there, or a store past the window's
    /// `private_max` (Firecracker's `AccessRefused`, TASK-70).
    #[error("extent store device: {0}")]
    AccessRefused(String),
}

pub type Result<T> = std::result::Result<T, Error>;
