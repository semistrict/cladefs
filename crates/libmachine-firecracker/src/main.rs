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
//! What it doesn't do it refuses as it starts, saying so on standard error: `pmem` devices, network
//! interfaces, and every request of the API but its socket answering (snapshots, pause and
//! migration need libmachine's snapshots on Hypervisor.framework).
//!
//! The guest's serial console goes to standard output. It exits as Firecracker does: 0 once the
//! guest resets or powers off (the tests' guest agent reboots it as a case ends), 158 once its
//! extent daemon goes (Firecracker's `CladefsdLost`), 1 otherwise, saying why on standard error and
//! in its log.

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod hvf;

fn main() {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    std::process::exit(hvf::main());
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        eprintln!("libmachine-firecracker is for Apple Silicon Macs; on Linux run Firecracker");
        std::process::exit(2);
    }
}
