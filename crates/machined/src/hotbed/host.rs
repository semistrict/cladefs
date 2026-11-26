// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What the node's host is, for its heartbeats' `Resources`: hotbed's names for its
//! architecture (Go's), its CPUs and memory, its kernel and model.

use super::proto;

/// The build a node says it runs. Not hotbed's "build N": the controller takes a node on another
/// build for one still upgrading and places nothing on it (hotbed's placement), which is what
/// machined's node is until hotbed places on machined's runtime (crates/machined/README.md).
pub(crate) const VERSION: &str = concat!("machined ", env!("CARGO_PKG_VERSION"));

/// The host's resources, as the node reports them.
pub(crate) fn resources() -> proto::Resources {
    proto::Resources {
        arch: arch().into(),
        cpus: std::thread::available_parallelism()
            .map_or(1, |n| i32::try_from(n.get()).unwrap_or(i32::MAX)),
        memory_bytes: memory().map_or(0, |m| i64::try_from(m).unwrap_or(i64::MAX)),
        kernel: kernel().unwrap_or_default(),
        machine: machine().unwrap_or_default(),
        version: VERSION.into(),
        ..proto::Resources::default()
    }
}

/// Go's name for the host's architecture (`GOARCH`), as hotbed's nodes report theirs.
fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    }
}

/// The kernel's release, as `uname -r`.
fn kernel() -> Option<String> {
    // SAFETY: `uname` fills the struct it is given, zeroed here.
    let mut name: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: `name` is a valid utsname to fill.
    if unsafe { libc::uname(&raw mut name) } != 0 {
        return None;
    }
    // SAFETY: `uname` NUL-terminates `release`.
    let release = unsafe { std::ffi::CStr::from_ptr(name.release.as_ptr()) };
    Some(release.to_string_lossy().into_owned())
}

#[cfg(target_os = "macos")]
fn sysctl(name: &std::ffi::CStr, value: *mut libc::c_void, len: &mut libc::size_t) -> bool {
    // SAFETY: `value` points at `len` writable bytes, or is null to ask the length.
    unsafe { libc::sysctlbyname(name.as_ptr(), value, len, std::ptr::null_mut(), 0) == 0 }
}

/// The host's memory, in bytes.
#[cfg(target_os = "macos")]
fn memory() -> Option<u64> {
    let mut memory = 0u64;
    let mut len = size_of::<u64>();
    sysctl(c"hw.memsize", (&raw mut memory).cast(), &mut len).then_some(memory)
}

/// The host's model, as the system names it ("Mac15,6").
#[cfg(target_os = "macos")]
fn machine() -> Option<String> {
    let mut len = 0;
    if !sysctl(c"hw.model", std::ptr::null_mut(), &mut len) {
        return None;
    }
    let mut model = vec![0u8; len];
    if !sysctl(c"hw.model", model.as_mut_ptr().cast(), &mut len) {
        return None;
    }
    let model = std::ffi::CStr::from_bytes_until_nul(&model).ok()?;
    Some(model.to_string_lossy().into_owned())
}

#[cfg(target_os = "linux")]
fn memory() -> Option<u64> {
    // SAFETY: `sysinfo` fills the struct it is given, zeroed here.
    let mut info: libc::sysinfo = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a valid sysinfo to fill.
    if unsafe { libc::sysinfo(&raw mut info) } != 0 {
        return None;
    }
    // machined's Linux is x86-64's: its unsigned long is a u64.
    info.totalram.checked_mul(u64::from(info.mem_unit))
}

#[cfg(target_os = "linux")]
fn machine() -> Option<String> {
    let name = std::fs::read_to_string("/sys/class/dmi/id/product_name").ok()?;
    Some(name.trim().to_owned()).filter(|name| !name.is_empty())
}
