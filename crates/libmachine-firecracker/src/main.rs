// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A Firecracker-compatible VMM on libmachine, for Apple Silicon Macs (board 13446): what cladefs's
//! guest tests run as CLADEFS_FIRECRACKER on a Mac (guest/tests' `vm.rs`), booting each case's VM
//! on Hypervisor.framework as cladefs's Firecracker would on Linux.
//!
//! It takes Firecracker's command line as the tests pass it (`--config-file`, `--no-api` or
//! `--api-sock`, `--log-path`, `--level`; `--enable-pci` and `--seccomp-filter` accepted, all its
//! devices being PCI and macOS having no seccomp) and its configuration file's:
//!
//! - `boot-source` (`kernel_image_path`, an arm64 `Image`; `boot_args`; `initrd_path`) and
//!   `machine-config` (`vcpu_count`, `mem_size_mib`);
//! - `drives`, virtio-blk disks, the root device first (`/dev/vda`, `root=/dev/vda` added to the
//!   kernel's command line as Firecracker adds it), `is_read_only` and `cache_type` as Firecracker
//!   has them;
//! - `cladefs`, cladefs's extent store devices, each on its cell (a directory, or a managed cell
//!   `TENANT/ID`, ID 0 making one) served by the extent daemon at its `cladefsd_socket`, with its
//!   DAX window and private pages' bound;
//! - `vsock`, served at its `uds_path` in Firecracker's protocol (`libmachine::vsock_unix`), the
//!   guest's connections to every port passed to `<uds_path>_<port>`;
//! - `balloon`, a virtio-balloon with free page reporting.
//!
//! Its API (`--api-sock`) serves what the guest tests ask of Firecracker's, with its status codes
//! (204, or 400 and a fault message), the machine's run stopped for each request and resumed
//! after it:
//!
//! - `PATCH /vm`, `Paused` or `Resumed`;
//! - `PUT /snapshot/create`, a full snapshot of the paused microVM: its state at `snapshot_path`
//!   (libmachine's snapshot, with what this VMM keeps of its devices' host sides: the vsock's
//!   `uds_path`, the extent daemon's socket) and the guest's memory at `mem_file_path`, flat and
//!   sparse as Firecracker's memory file (libmachine's RAM file of its own); cladefs's
//!   `for_migration` as Firecracker's;
//! - `PUT /snapshot/load`, in a process started with `--api-sock` and no configuration: the
//!   snapshot's microVM, from a `File` memory backend (or `mem_file_path`), with cladefs's
//!   `cladefs_overrides` (each extent store device's cell and daemon) and `vsock_override`,
//!   resumed if `resume_vm`, else paused;
//! - `PATCH /balloon`, the balloon's target;
//! - cladefs's memory cells (board 13994, `memory`): `PUT /snapshot/create` with `memory_cell`
//!   (into a new cell of a cold-booted microVM's resident pages, or the restored microVM's own,
//!   ephemeral or not, answered 200 with the cell and its version as Firecracker answers), `Diff`
//!   snapshots of a microVM restored from one (what its guest wrote since the last snapshot),
//!   and `PUT /snapshot/load` from a `Cell` memory backend (`memory_cell`, its `diff_files`
//!   applied), the guest's RAM the cell's window;
//! - cladefs's `PATCH /cladefs/extents`, the extent store devices' cells `Released` for a live
//!   migration's hand-over or `Open` again, the microVM paused, and never resumed released.
//!
//! What it doesn't do it refuses, saying so on standard error: network interfaces and pmem root
//! devices as it starts (pmem data devices it maps into the guest, as Firecracker does); `Diff`
//! snapshots of a cold-booted microVM, and every other request of the API's.
//!
//! The guest's serial console goes to standard output. It exits as Firecracker does: 0 once the
//! guest resets or powers off (the tests' guest agent reboots it as a case ends), 158 once its
//! extent daemon goes (Firecracker's `CladefsdLost`), 159 once a guest's access to a DAX window is
//! refused for good (`AccessRefused`), 1 otherwise, saying why on standard error and in its log.

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod hvf;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod memory;

fn main() {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    std::process::exit(hvf::main());
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        eprintln!("libmachine-firecracker is for Apple Silicon Macs; on Linux run Firecracker");
        std::process::exit(2);
    }
}
