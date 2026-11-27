// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! VM-process wire protocol, codec, client, and memory-cell client for cladefsd.

#![deny(clippy::undocumented_unsafe_blocks)]

mod backend;
pub mod client;
pub mod codec;
pub mod device;
pub mod in_flight;
// cladefs: a VM's memory as a cell, restored as the cell's window: userfaultfd's on Linux, a
// guest's RAM on Hypervisor.framework on macOS.
pub mod memory_cell;
pub mod wire;

pub use client::{CellClient, DaemonConnection, Unmapped, spawn_on_replies};
pub use device::DaemonDevice;
