// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! VM-process wire protocol, codec, client, and memory-cell client for cladefsd.

#![deny(clippy::undocumented_unsafe_blocks)]

#[cfg(target_os = "linux")]
pub mod client;
#[cfg(any(target_os = "linux", madsim))]
pub mod codec;
#[cfg(target_os = "linux")]
pub mod device;
#[cfg(any(target_os = "linux", madsim))]
pub mod in_flight;
#[cfg(target_os = "linux")]
pub mod memory_cell;
#[cfg(any(target_os = "linux", madsim))]
pub mod wire;

#[cfg(target_os = "linux")]
pub use client::{CellClient, DaemonConnection, Unmapped, spawn_on_replies};
#[cfg(target_os = "linux")]
pub use device::DaemonDevice;
