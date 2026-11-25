// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefsctl's cells in a bucket: a file bucket in a test's directory, in every run, and a real
//! S3 server's, ignored but by `scripts/test-rustfs.sh`, which starts RustFS and runs those with
//! `CLADEFS_S3_URL` naming a bucket of it, and the `AWS_*` variables to reach it. Each S3 test was
//! that alone until TASK-149.20, which found `cladefsctl take` of a new copy at a later placement
//! refused (`ESTALE`): nothing ran them.

#![cfg(target_os = "linux")]

use bucket_cell::object_bucket::ObjectBucket;
use cladefs::{CellClient, CellId, ExtentId, VolatileSlice, VolatileSliceMut};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn pattern(seed: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| seed ^ (i % 251) as u8).collect()
}

struct Daemon {
    vms: PathBuf,
    control: PathBuf,
    child: std::process::Child,
    _dir: tempfile::TempDir,
}

impl Daemon {
    /// Starts `cladefsd` against the bucket at `url` with a managed disk root so `cladefsctl`'s
    /// live-cell commands run through its control API (board 11705).
    fn start(url: &str, token: u64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("disk0");
        std::fs::create_dir_all(&disk).unwrap();
        let vms = dir.path().join("vms.sock");
        let control = dir.path().join("control.sock");
        let child = Command::new(env!("CARGO_BIN_EXE_cladefsd"))
            .arg(&vms)
            .env("CLADEFSD_GRPC", &control)
            .env("CLADEFSD_ROOTS", &disk)
            .env("CLADEFSD_BUCKET", url)
            .env("CLADEFSD_TOKEN", token.to_string())
            .env("CLADEFSD_DISK_HEADROOM", "0")
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while std::os::unix::net::UnixStream::connect(&control).is_err() {
            assert!(Instant::now() < deadline, "cladefsd never served {}", control.display());
            std::thread::sleep(Duration::from_millis(20));
        }
        Self { vms, control, child, _dir: dir }
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_cladefsctl"))
            .args(args)
            .env("CLADEFSD_GRPC", &self.control)
            .env_remove("CLADEFSD_BUCKET")
            .env_remove("CLADEFSD_TOKEN")
            .env_remove("CLADEFS_CONFIG")
            .output()
            .unwrap()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Runs cladefsctl with `args` through `daemon`: what it prints.
fn cell(daemon: &Daemon, args: &[&str]) -> String {
    let output = daemon.run(args);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "cladefsctl {args:?}: {stderr}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// Runs cladefsctl with `args` through `daemon`, retrying briefly if the daemon is still finishing
/// a VM disconnect (`EBUSY`).
fn cell_once_closed(daemon: &Daemon, args: &[&str]) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let output = daemon.run(args);
        if output.status.success() {
            return String::from_utf8(output.stdout).unwrap().trim().to_owned();
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.contains("EBUSY") || Instant::now() >= deadline {
            panic!("cladefsctl {args:?}: {stderr}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn read(client: &CellClient, extent: ExtentId, len: usize) -> Vec<u8> {
    let mut data = vec![0; len];
    client.read(extent, 0, &mut [VolatileSliceMut::from(&mut data[..])]).unwrap();
    data
}

/// The URL of the S3 bucket `CLADEFS_S3_URL` names.
fn s3_url() -> String {
    std::env::var("CLADEFS_S3_URL").expect("CLADEFS_S3_URL names the bucket to use")
}

/// A file bucket in `dir`, and its URL.
fn file_bucket(dir: &tempfile::TempDir) -> String {
    format!("file://{}", dir.path().display())
}

#[tokio::test(flavor = "multi_thread")]
async fn forks_and_takes_cells() {
    let bucket = tempfile::tempdir().unwrap();
    forks_and_takes_cells_in(&file_bucket(&bucket)).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs an S3 bucket: scripts/test-rustfs.sh"]
async fn rustfs_forks_and_takes_cells() {
    forks_and_takes_cells_in(&s3_url()).await;
}

/// A cell forked with cladefsctl is a fork of its cell in the bucket at `url`, and a cell taken
/// with it onto another host's empty disk is read there from the bucket; deleted with it, the fork
/// goes, and then its parent.
async fn forks_and_takes_cells_in(url: &str) {
    let url = url.to_owned();
    let bucket = Arc::new(ObjectBucket::from_url(&url).unwrap());
    let daemon = Daemon::start(&url, 1);
    let capacity = (1u64 << 30).to_string();

    let parent_str = cell(&daemon, &["create", &capacity]);
    let parent_cell: CellId = parent_str.parse().unwrap();
    let parent = CellClient::connect_cell(&daemon.vms, parent_cell).unwrap();
    let data = pattern(7, 5 << 20);
    let extent = ExtentId { cell: parent_cell.id, sequence: 1 };
    parent.write(extent, 0, &[VolatileSlice::from(&data[..])]).unwrap();
    parent.commit(Some(b"root")).unwrap();
    drop(parent);

    let child = cell_once_closed(&daemon, &["fork", &parent_str]);
    let child_cell: CellId = child.parse().unwrap();
    let record = bucket_cell::head::get(&*bucket, child_cell).await;
    assert_eq!(record.unwrap().unwrap().origin().map(|origin| origin.parent), Some(parent_cell));
    let forked = CellClient::connect_cell(&daemon.vms, child_cell).unwrap();
    assert_eq!(forked.cell(), child_cell.id);
    assert_eq!(read(&forked, extent, data.len()), data);
    drop(forked);
    cell_once_closed(&daemon, &["sync", &child]);

    let elsewhere_daemon = Daemon::start(&url, 2);
    cell(&elsewhere_daemon, &["take", &capacity, &child, "2"]);
    let elsewhere = CellClient::connect_cell(&elsewhere_daemon.vms, child_cell).unwrap();
    assert_eq!(elsewhere.root().unwrap(), b"root");
    assert_eq!(read(&elsewhere, extent, data.len()), data);
    drop(elsewhere);

    // The fork deleted, then its parent.
    cell_once_closed(&elsewhere_daemon, &["delete", &child]);
    cell_once_closed(&daemon, &["delete", &parent_str]);
}

/// Runs cladefsctl with `args` through `daemon`, expecting it to fail: what it says.
fn cell_fails(daemon: &Daemon, args: &[&str]) -> String {
    let output = daemon.run(args);
    assert!(!output.status.success(), "cladefsctl {args:?} succeeded");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn places_and_syncs_cells() {
    let bucket = tempfile::tempdir().unwrap();
    places_and_syncs_cells_in(&file_bucket(&bucket)).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs an S3 bucket: scripts/test-rustfs.sh"]
async fn rustfs_places_and_syncs_cells() {
    places_and_syncs_cells_in(&s3_url()).await;
}

/// TASK-39.7: `cladefsctl sync` returns once a cell's commits are in the bucket at `url`; a cell's
/// copy placed earlier than the placement that took its cell last takes it no more, and placed
/// later, does.
async fn places_and_syncs_cells_in(url: &str) {
    let url = url.to_owned();
    let bucket = Arc::new(ObjectBucket::from_url(&url).unwrap());
    let daemon = Daemon::start(&url, 1);
    let capacity = (1u64 << 30).to_string();

    let made = cell(&daemon, &["create", &capacity]);
    let cell_id: CellId = made.parse().unwrap();
    let open = CellClient::connect_cell(&daemon.vms, cell_id).unwrap();
    let extent = ExtentId { cell: cell_id.id, sequence: 1 };
    open.write(extent, 0, &[VolatileSlice::from(&pattern(3, 1 << 20)[..])]).unwrap();
    open.commit(Some(b"synced")).unwrap();
    drop(open);
    cell_once_closed(&daemon, &["place", &made, "5"]);
    cell(&daemon, &["sync", &made]);
    let state = bucket_cell::recover(&*bucket, cell_id, None).await.unwrap().state;
    assert_eq!(state.root, Some(b"synced".to_vec()));

    let second = Daemon::start(&url, 1);
    // Refused as it takes the cell's head, which a later placement holds, leaving nothing
    // (TASK-165.9: before, it took the cell and its sync was refused); taken placed later, its
    // head taken with that placement's token as it is made (TASK-149.20).
    let stale = cell_fails(&second, &["take", &capacity, &made, "3"]);
    assert!(stale.contains("ESTALE"), "{stale}");
    cell(&second, &["take", &capacity, &made, "6"]);
    cell(&second, &["sync", &made]);
    let later = CellClient::connect_cell(&second.vms, cell_id).unwrap();
    assert_eq!(later.root().unwrap(), b"synced");
    assert_eq!(read(&later, extent, 1 << 20), pattern(3, 1 << 20));
}
