// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! libmachine: an embeddable virtual machine monitor.
//!
//! It runs on x86-64 Linux, with KVM, and on arm64 macOS, with Hypervisor.framework (Apple
//! Silicon, macOS 15 and later: board 13063). Elsewhere the crate is empty.
//!
//! Two kinds of machine:
//! - [`Machine::boot_linux`] builds a PC (KVM's in-kernel interrupt
//!   controllers, hardware-reduced ACPI, a 16550 console) and boots an ELF
//!   `vmlinux` through PVH or a `bzImage` through the 64-bit boot protocol.
//!   [`Machine::run`] returns when the guest powers off or resets.
//!   [`MachineConfig::vcpus`] sets how many vCPUs it has, each on a host
//!   thread of its own.
//! - [`Machine::new`] builds a bare machine whose single vCPU starts in
//!   16-bit real mode, for instruction-level tests; every access no device
//!   handles is reported to the caller.
//!
//! Guest RAM sits around the 32-bit MMIO hole (up to 3 GiB at 0, the rest
//! from 4 GiB); devices map host memory into the MMIO windows at runtime
//! ([`Machine::memory_slots`]), and [`Machine::memory_layout`] describes the
//! whole map.
//!
//! Every PC machine has a virtio-vsock device whose host end is in this
//! process ([`Machine::vsock`]), and [`agent`] talks to libmachine's guest
//! agent over it: commands with their stdio, files in and out.
//!
//! A PC machine may have a network ([`MachineConfig::network`]): a
//! virtio-net card whose other end is a userspace TCP/IP stack in this
//! process, the guest's connections made from the host only where the
//! caller's [`net::EgressPolicy`] allows.
//!
//! The guest's CPU model is explicit ([`CpuModel`]), with a fingerprint for
//! snapshot compatibility ([`Machine::cpu_fingerprint`]).
//!
//! [`sandbox`] (the `sandbox` feature, on by default) is the high-level,
//! async API on tokio: a [`sandbox::Sandbox`] boots a guest with the agent
//! and runs commands and copies files from async code, many per process.
//!
//! A stopped PC machine can be snapshotted to a file ([`Machine::snapshot`])
//! and any number of machines restored from it ([`Snapshot`],
//! [`Machine::restore`]), each with its RAM a copy-on-write mapping of the
//! file, a fresh RNG seed and the right time.
//!
//! A machine may have cladefs's extent store devices ([`extent`]): its guest's Fxfs on a cell of
//! cladefs's extent daemon, embedded in the process, which every machine of the process shares
//! ([`MachineConfig::extent_daemon`], [`MachineConfig::extent_stores`]), its files' data mapped
//! through a DAX window on KVM.
//!
//! A machine counts the vCPU exits it takes, by kind
//! ([`Machine::exit_counts`]), so that tests can bound them.
#![cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
))]
// On macOS, snapshots and suspensions are KVM's still (board 13399): their code builds there,
// unused.
#![cfg_attr(
    target_os = "macos",
    allow(dead_code, reason = "snapshots and suspensions come to HVF in board 13399")
)]

pub mod agent;
mod arch;
mod backend;
mod cpu;
mod devices;
mod error;
mod exits;
pub mod extent;
mod machine;
mod memory;
pub mod net;
#[cfg(all(feature = "sandbox", target_os = "linux", target_arch = "x86_64"))]
pub mod sandbox;
#[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
mod smp_tests;
mod snapshot;
mod stop;
mod suspend;
mod sys;
mod vcpus;
pub mod vsock_unix;

#[cfg(target_os = "linux")]
pub use backend::{HostMemory, MemorySlot, MemorySlots};
pub use cpu::CpuModel;
#[cfg(target_arch = "x86_64")]
pub use cpu::CpuidEntry;
pub use devices::virtio::balloon::Balloon;
pub use devices::virtio::blk::Disk;
pub use devices::virtio::vsock::{
    GuestStopped, Vsock, VsockListener, VsockStream, is_guest_stopped,
};
pub use error::{Error, Result};
pub use exits::ExitCounts;
pub use extent::{CellId, ExtentCache, ExtentCell, ExtentDaemon, ExtentDaemonConfig, ExtentStore};
#[cfg(target_arch = "x86_64")]
pub use machine::Registers;
pub use machine::{ExitReason, LinuxBoot, Machine, MachineConfig, Run, Waited};
pub use memory::{MemoryLayout, MemoryRegion, RegionKind};
#[cfg(target_os = "linux")]
pub use snapshot::{FORMAT_VERSION as SNAPSHOT_FORMAT_VERSION, Snapshot};
pub use stop::StopHandle;
pub use suspend::Woken;
