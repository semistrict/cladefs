// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! libmachine: an embeddable virtual machine monitor for cladefs's guests on macOS.
//!
//! It runs on arm64 macOS, with Hypervisor.framework (Apple Silicon, macOS 15 and later: board
//! 13063). Elsewhere the crate is empty: Firecracker is cladefs's VMM on Linux.
//!
//! Two kinds of machine:
//! - [`Machine::boot_linux`] builds the arm64 board (the framework's GICv3 and timers, PSCI, a
//!   16550 console, a PCI bus of virtio devices) and boots an arm64 `Image`. [`Machine::run`]
//!   returns when the guest powers off or resets. [`MachineConfig::vcpus`] sets how many vCPUs
//!   it has, each on a host thread of its own.
//! - [`Machine::new`] builds a bare machine whose single vCPU starts at EL1 at an address you
//!   choose, for instruction-level tests; every access no device handles is reported to the
//!   caller.
//!
//! Every board machine has a virtio-vsock device whose host end is in this process
//! ([`Machine::vsock`]), and [`agent`] talks to libmachine's guest agent over it: commands with
//! their stdio, files in and out.
//!
//! A board machine may have a network ([`MachineConfig::network`]): a virtio-net card whose
//! other end is a userspace TCP/IP stack in this process, the guest's connections made from the
//! host only where the caller's [`net::EgressPolicy`] allows.
//!
//! A stopped board machine can be snapshotted to a file ([`Machine::snapshot_with`]) and any
//! number of machines restored from it ([`Snapshot`], [`Machine::restore`]), each with its RAM a
//! copy-on-write mapping of the file, or of memory its caller keeps ([`Machine::restore_onto`]).
//!
//! A machine may have cladefs's extent store devices ([`extent`]): its guest's Fxfs on a cell of
//! cladefs's extent daemon, embedded in the process, which every machine of the process shares
//! ([`MachineConfig::extent_daemon`], [`MachineConfig::extent_stores`]), its files' data mapped
//! through a DAX window.
//!
//! A machine counts the vCPU exits it takes, by kind ([`Machine::exit_counts`]), so that tests
//! can bound them.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

pub mod agent;
mod arch;
mod backend;
mod devices;
mod error;
mod exits;
pub mod extent;
mod machine;
mod memory;
pub mod net;
mod snapshot;
mod stop;
mod sys;
mod vcpus;
pub mod vsock_unix;

pub use backend::GuestRam;
pub use devices::virtio::balloon::Balloon;
pub use devices::virtio::blk::Disk;
pub use devices::virtio::pmem::Pmem;
pub use devices::virtio::vsock::{
    GuestStopped, Vsock, VsockListener, VsockStream, is_guest_stopped,
};
pub use error::{Error, Result};
pub use exits::ExitCounts;
pub use extent::{CellId, ExtentCache, ExtentCell, ExtentDaemon, ExtentDaemonConfig, ExtentStore};
pub use machine::{ExitReason, LinuxBoot, Machine, MachineConfig, Run, SnapshotOptions, Waited};
pub use memory::{MemoryLayout, MemoryRegion, RegionKind};
pub use snapshot::{FORMAT_VERSION as SNAPSHOT_FORMAT_VERSION, Snapshot, SnapshotRam};
pub use stop::StopHandle;
