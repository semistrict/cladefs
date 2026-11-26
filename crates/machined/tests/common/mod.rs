// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What machined's end-to-end tests share: whether this host runs VMs, and the guest they boot.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// Whether this host runs VMs (else a note): /dev/kvm, accessible, on Linux;
/// Hypervisor.framework (`kern.hv_support`) on macOS, which GitHub's macOS runners, themselves
/// VMs, lack (board 13847).
pub fn hypervisor() -> bool {
    if cfg!(target_os = "linux") {
        if std::fs::OpenOptions::new().read(true).write(true).open("/dev/kvm").is_err() {
            eprintln!("skipping: /dev/kvm is not accessible");
            return false;
        }
        return true;
    }
    let support = std::process::Command::new("sysctl").args(["-n", "kern.hv_support"]).output();
    if !support.is_ok_and(|out| out.status.success() && out.stdout.trim_ascii() == b"1") {
        eprintln!("skipping: this host has no Hypervisor.framework (kern.hv_support)");
        return false;
    }
    true
}

/// The guest's kernel and initramfs, if this host can boot them (else a note).
pub fn guest() -> Option<(PathBuf, PathBuf)> {
    if !hypervisor() {
        return None;
    }
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let (default_dir, default_kernel) = if cfg!(target_os = "linux") {
        (workspace.join("target/guest"), None)
    } else {
        let dir = workspace.join("target/arm64-guest");
        let kernel = dir.join("Image");
        (dir, Some(kernel))
    };
    let kernel = std::env::var_os("CLADEFS_GUEST_KERNEL").map(PathBuf::from).or(default_kernel);
    let dir = std::env::var_os("LIBMACHINE_GUEST_DIR").map_or(default_dir, PathBuf::from);
    let initramfs = dir.join("initramfs.cpio");
    match kernel {
        Some(kernel) if kernel.is_file() && initramfs.is_file() => Some((kernel, initramfs)),
        kernel => {
            eprintln!(
                "skipping: no guest kernel ({kernel:?}; CLADEFS_GUEST_KERNEL) or initramfs ({}): \
                 scripts/guest-kernel.sh and crates/libmachine/scripts/build-test-guest.sh, or \
                 on macOS crates/libmachine/scripts/arm64-guest.sh",
                initramfs.display()
            );
            None
        }
    }
}
