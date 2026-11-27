// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What the guest tests share: the arm64 guest they boot, the one VM a process has, and the
//! guest programs of the bare machine's tests, assembled (dynasm, never raw bytes).
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

/// Hypervisor.framework gives a process one VM: one test's machine at a time.
pub fn one_machine() -> MutexGuard<'static, ()> {
    static ONE: Mutex<()> = Mutex::new(());
    ONE.lock().unwrap_or_else(|e| e.into_inner())
}

/// cladefs's arm64 kernel and the test initramfs, if both are there (else a note): the kernel is
/// `CLADEFS_GUEST_KERNEL`, else `target/arm64-guest/Image`; the initramfs is in
/// `LIBMACHINE_GUEST_DIR`, else `target/arm64-guest` (`crates/libmachine/scripts/arm64-guest.sh`
/// fetches both).
pub fn guest() -> Option<(PathBuf, PathBuf)> {
    let default = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/arm64-guest");
    let kernel = std::env::var_os("CLADEFS_GUEST_KERNEL")
        .map_or_else(|| default.join("Image"), PathBuf::from);
    let initrd = std::env::var_os("LIBMACHINE_GUEST_DIR")
        .map_or(default, PathBuf::from)
        .join("initramfs.cpio");
    if !kernel.is_file() || !initrd.is_file() {
        eprintln!(
            "skipping: no arm64 guest at {} and {} (run crates/libmachine/scripts/arm64-guest.sh)",
            kernel.display(),
            initrd.display()
        );
        return None;
    }
    Some((kernel, initrd))
}

/// Where a bare machine's program goes: RAM's start.
pub const ENTRY: u64 = 0x8000_0000;

/// Assembles `program`, for `ENTRY`.
pub fn assemble(
    program: impl FnOnce(&mut dynasmrt::VecAssembler<dynasmrt::aarch64::Aarch64Relocation>),
) -> Vec<u8> {
    let mut ops = dynasmrt::VecAssembler::new(ENTRY as usize);
    program(&mut ops);
    ops.finalize().expect("assemble the guest program")
}

/// Whether this host runs Hypervisor.framework's VMs (`kern.hv_support`): GitHub's macOS runners,
/// themselves VMs, don't (board 13847). Tests return early, with a note, where it doesn't.
pub fn hypervisor() -> bool {
    let support = std::process::Command::new("sysctl").args(["-n", "kern.hv_support"]).output();
    let ok = support.is_ok_and(|out| out.status.success() && out.stdout.trim_ascii() == b"1");
    if !ok {
        eprintln!("skipping: this host has no Hypervisor.framework (kern.hv_support)");
    }
    ok
}
