// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The virtio extent-store device (backlog doc-4, device type `0x4658`), but its virtio
//! transport: one copy of it, which every VMM hosting it serves, cladefs's Firecracker
//! (vendor/firecracker) and libmachine (crates/libmachine), so that its versioned protocol,
//! which never migrates, can't drift between them.
//!
//! A guest that runs Fxfs itself stores its filesystem through this device, as extents its Fxfs
//! names: immutable pieces of data, written once and sealed by the next commit, freed whole, and
//! a root naming where the filesystem starts. cladefs's extent daemon, which the device connects
//! to, keeps them in an cladefs extent store on local disk, and maps sealed extents into the
//! device's DAX window, its shared memory region 0, from its cache of their pages, which the
//! host's VMs share.
//!
//! - [`protocol`]: the requests, flags, features and configuration space, and the version a
//!   driver and a snapshot are checked against: `cladefs::extent_requests`'s, which a local
//!   process's ring to the extent daemon carries too (board 14378).
//! - [`Config`]: a device's configuration, as a VMM's user gives it.
//! - [`Device`]: a device's cell, configuration space, notices, DAX window (on macOS mapped by
//!   the VMM as cladefs's stage says, [`window_fault`] taking the guest's faults, board 13069) and
//!   snapshots, which a VMM's adapter holds beside its transport's state.
//! - [`Buffers`], [`handle`], [`Guest`] and [`Requests`]: serving a request whose buffers the
//!   adapter took off a queue, over any [`GuestBackend`], with the interpreter every transport of
//!   doc-4's requests shares, in the guest's [`Dialect`]; [`Workers`], a thread each queue, taking
//!   requests off a [`RequestQueue`] of the VMM's transport as its event (an eventfd, or a pipe on
//!   macOS) is notified, and returning them.
//! - [`RequestQueue`], [`QueueInterrupt`], [`ConfigInterrupt`], [`WindowVm`] and
//!   [`PrefaultGuestMemory`]: what only the VMM can do,
//!   which its adapter implements.
//! - [`persist`]: the device in a snapshot of its VM, as plain values.
//! - [`vcpu`] (Linux): a VMM's vCPUs' retries of a guest's accesses failing as the window changes,
//!   and what they tell of one that keeps failing; [`kvm`] (x86-64 Linux): KVM's prefault.
//!
//! README.md lists what a VMM's adapter does.

// Every unsafe block says why it is sound, other threads included (TASK-444.4).
#![deny(clippy::undocumented_unsafe_blocks)]

mod config;
mod device;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod kvm;
mod notices;
pub mod persist;
mod requests;
mod sync;
#[cfg(target_os = "linux")]
pub mod vcpu;
mod window;
mod workers;

pub use config::{CacheType, Config, ConfigError, PRIVATE_MAX_DEFAULT, private_max};
pub use device::{ActivateError, Device, Error, OnLost, ReleaseError, ShmRegion};
pub use notices::{ConfigInterrupt, Notices, Resizing, StatsAsks};
pub use persist::SnapshotMode;
// cladefs: doc-4's protocol, its buffers, backends and interpreter, which a local process's ring to
// the extent daemon shares (board 14378).
pub use cladefs::extent_requests::{
    Backend, Buffers, BuffersError, Dialect, Guest, GuestBackend, GuestNotices, handle, protocol,
};
pub use requests::{Requests, Served};
#[cfg(target_os = "linux")]
pub use window::dax_window_at;
pub use window::{DaxWindowError, PrefaultGuestMemory, WindowVm};
#[cfg(not(target_os = "linux"))]
pub use window::{is_window, window_fault};
pub use workers::{Popped, QueueInterrupt, RequestQueue, WorkerQueue, Workers};

/// An error of the VMM's, which the device passes on.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
