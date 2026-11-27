// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A vhost-user back-end (QEMU's `docs/interop/vhost-user.rst`) for a device the extent daemon
//! serves: the extent device, virtio type 0x4658 (doc-4, board 14537). Linux alone.
//!
//! - [`protocol`]: the front-end's messages, decoded on checked cursors into values that mean
//!   something; the front-end is untrusted (AGENTS.md, "Trust boundaries").
//! - [`memory`]: guest memory as the memory table gives it, memfd regions, reached with `pread`
//!   and `pwrite`, only the vrings' pages mapped (plan v3, board 14378).
//! - [`ring`]: the split ring, its device half checking every descriptor.
//! - [`Channel`] and [`Device`]: the back-end, a thread a channel, serving the messages, hearing
//!   the kicks, taking chains for the device to serve ([`Chain`]) and writing the calls; and
//!   saying the device's configuration changes on the back-end channel ([`Notifier`], board
//!   14603).
//! - `front_end` (feature `testing`): a minimal front-end for tests, which plays the VMM and the
//!   guest's driver over memfd guest RAM.
//!
//! What these milestones leave for later, room kept for each: `DEVICE_STATE` (snapshots, board
//! 14605), doc-32's messages on the back-end channel and `SHMEM_LAZY` (the DAX window, board
//! 14606), the memory cell's private store and private map in the memory table (board 14604), and
//! dirty logging (`VHOST_F_LOG_ALL`).

#![cfg(target_os = "linux")]
// Every unsafe block says why it is sound, other threads included (TASK-444.4).
#![deny(clippy::undocumented_unsafe_blocks)]

// cladefs: with `loom`, the ring alone, on loom's atomics, for its models
// (`cargo nextest run -p vhost_user --features loom --lib loom::`).
#[cfg(not(feature = "loom"))]
mod backend;
#[cfg(not(feature = "loom"))]
mod backend_channel;
#[cfg(not(feature = "loom"))]
mod eventfd;
#[cfg(all(any(test, feature = "testing"), not(feature = "loom")))]
pub mod front_end;
pub mod memory;
pub mod protocol;
#[cfg(not(feature = "loom"))]
mod queue;
pub mod ring;
#[cfg(not(feature = "loom"))]
mod socket;
mod sync;

#[cfg(not(feature = "loom"))]
pub use backend::{Channel, Device, Notifier, panics};
#[cfg(not(feature = "loom"))]
pub use queue::Chain;
