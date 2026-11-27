// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-39.2: a VM process reaches the memory of its extent daemon alone, which caches the
//! extents of the cells the daemon serves: another daemon's cached pages are not in what a VM
//! process of this one can map. (One daemon serves every tenant, each in an arena of its memory of
//! its own: tests/tenants.rs, TASK-43.)
//!
//! Two daemons, each its own process with its own kernel, serve a cell each. Each cell's extent,
//! written, committed and read through a window, is in its daemon's physical memory, which the
//! VM process of that cell holds, and not in the other's.

#![cfg(target_os = "linux")]

mod common;

use cladefs::CellClient;
use common::{cache, holds, pattern};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// A daemon of its own, serving at a socket in `dir`, once it does, owning a directory of its own
/// there, `dir/NAME`, which its cells are kept in: one daemon owns a disk (board 14305).
fn daemon(dir: &Path, name: &str) -> (PathBuf, KillOnDrop) {
    let socket = dir.join(format!("{name}.sock"));
    let owned = dir.join(name);
    std::fs::create_dir(&owned).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_cladefsd"))
        .arg(&socket)
        .env("CLADEFSD_ROOTS", &owned)
        .env("CLADEFSD_BUCKET", "memory:///cells")
        .env("CLADEFSD_TOKEN", "1")
        .spawn()
        .unwrap();
    let child = KillOnDrop(child);
    let deadline = Instant::now() + Duration::from_secs(30);
    while std::os::unix::net::UnixStream::connect(&socket).is_err() {
        assert!(Instant::now() < deadline, "the daemon {name} never served");
        std::thread::sleep(Duration::from_millis(20));
    }
    (socket, child)
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn vm_processes_reach_their_daemons_memory_alone() {
    let dir = tempfile::tempdir().unwrap();
    let (one, _one) = daemon(dir.path(), "one");
    let (two, _two) = daemon(dir.path(), "two");
    let ours = CellClient::connect_new(&one, 0, 1 << 24).unwrap();
    let theirs = CellClient::connect_new(&two, 0, 1 << 24).unwrap();
    let (our_bytes, their_bytes) = (pattern(1), pattern(2));
    cache(&ours, &our_bytes);
    cache(&theirs, &their_bytes);

    assert!(holds(ours.physical_memory(), &our_bytes), "our pages are not in our memory");
    assert!(!holds(ours.physical_memory(), &their_bytes), "their pages are in our memory");
    assert!(holds(theirs.physical_memory(), &their_bytes), "their pages are not in theirs");
    assert!(!holds(theirs.physical_memory(), &our_bytes), "our pages are in theirs");
}
