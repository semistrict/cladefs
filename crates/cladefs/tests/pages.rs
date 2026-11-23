// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Tests of physical memory's configuration, which works as nextest runs each test in its own
//! process, whose physical memory is not yet in use.

use cladefs::{Errno, pages};

const MIB: u64 = 1024 * 1024;

#[test]
fn configure_before_physical_memory_is_used() {
    pages::configure(pages::Config::with_memory(256 * MIB)).expect("configure failed");
    assert_eq!(cladefs::slabs::memory_bytes(), 256 * MIB);
    drop(pages::Arena::new().unwrap());
}

#[test]
fn configure_once_physical_memory_is_used_fails() {
    drop(pages::Arena::new().unwrap());
    let error = pages::configure(pages::Config::default()).expect_err("configured");
    assert_eq!(error.to_string(), "physical memory has already been configured or used");
    assert_eq!(error.errno(), Some(Errno::EBUSY));
}

#[test]
fn configure_once() {
    assert_eq!(pages::Config::default(), pages::Config::with_memory(1024 * MIB));
    pages::configure(pages::Config::with_memory(4096 * MIB)).expect("configure failed");
    assert_eq!(cladefs::slabs::memory_bytes(), 4096 * MIB);
    let error = pages::configure(pages::Config::default()).expect_err("configured");
    assert_eq!(error.errno(), Some(Errno::EBUSY));
}

#[test]
fn memory_of_part_of_a_slab_fails() {
    let memory = 3 * MIB + MIB / 2;
    let error = pages::configure(pages::Config::with_memory(memory)).expect_err("configured");
    assert_eq!(
        error.to_string(),
        "invalid physical memory size: 3670016 bytes, not whole slabs of 1048576"
    );
    assert_eq!(error.errno(), Some(Errno::EINVAL));
}

/// doc-26: memory of more slabs than the host's `vm.max_map_count` would have mapped in chunks
/// (TASK-79.37's bound) is configured: the daemon maps none of it.
#[cfg(target_os = "linux")]
#[test]
fn memory_past_what_the_host_maps_is_configured() {
    let max_map_count = std::fs::read_to_string("/proc/sys/vm/max_map_count").unwrap();
    let past = (max_map_count.trim().parse::<u64>().unwrap() + 1) * 2 * MIB;
    pages::configure(pages::Config::with_memory(past)).expect("configure failed");
    assert_eq!(cladefs::slabs::memory_bytes(), past);
}
