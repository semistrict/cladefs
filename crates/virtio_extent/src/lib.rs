// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The virtio extent-store device (backlog doc-4, device type `0x4658`), but its virtio
//! transport: one copy of it, which every VMM hosting it serves, cladefs's Firecracker
//! (vendor/firecracker) and libmachine, so that its versioned protocol, which never migrates,
//! can't drift between them.
//!
//! A guest that runs Fxfs itself stores its filesystem through this device, as extents its Fxfs
//! names: immutable pieces of data, written once and sealed by the next commit, freed whole, and
//! a root naming where the filesystem starts. cladefs's extent daemon, which the device connects
//! to, keeps them in an cladefs extent store on local disk, and maps sealed extents into the
//! device's DAX window, its shared memory region 0, from its cache of their pages, which the
//! host's VMs share.
//!
//! - [`protocol`]: the requests, flags, features and configuration space, and the version a
//!   driver and a snapshot are checked against.
//! - [`Config`]: a device's configuration, as a VMM's user gives it.
//! - [`Device`] (Linux): a device's store, configuration space, notices, DAX window and
//!   snapshots, which a VMM's adapter holds beside its transport's state.
//! - [`Buffers`], [`handle`] and, on Linux, [`Requests`]: serving a request whose buffers the
//!   adapter took off a queue, over any [`Backend`]; on Linux, [`Workers`], a thread each queue,
//!   taking requests off a [`RequestQueue`] of the VMM's transport and returning them.
//! - [`RequestQueue`], [`QueueInterrupt`], [`ConfigInterrupt`], [`WindowVm`] and
//!   [`PrefaultGuestMemory`]: what only the VMM can do,
//!   which its adapter implements.
//! - [`persist`]: the device in a snapshot of its VM, as plain values.
//!
//! README.md lists what a VMM's adapter does.

mod backend;
mod buffers;
mod config;
#[cfg(target_os = "linux")]
mod device;
mod notices;
pub mod persist;
pub mod protocol;
mod requests;
mod sync;
#[cfg(target_os = "linux")]
mod window;
#[cfg(target_os = "linux")]
mod workers;

pub use backend::Backend;
pub use buffers::{Buffers, BuffersError};
pub use config::{CacheType, Config, ConfigError, PRIVATE_MAX_DEFAULT, private_max};
#[cfg(target_os = "linux")]
pub use device::{ActivateError, Device, Error, OnLost, ShmRegion};
pub use notices::{ConfigInterrupt, Following, Notices, Resizing, StatsAsks};
#[cfg(target_os = "linux")]
pub use requests::Requests;
pub use requests::{Served, handle};
#[cfg(target_os = "linux")]
pub use window::{DaxWindowError, PrefaultGuestMemory, WindowVm, dax_window_at};
#[cfg(target_os = "linux")]
pub use workers::{Popped, QueueInterrupt, RequestQueue, WorkerQueue, Workers};

/// An error of the VMM's, which the device passes on.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
