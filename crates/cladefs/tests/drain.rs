// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-141.4: SIGTERM drains the daemon before it exits (`cladefs::cladefsd::drain`): a VM process
//! still connected is told its daemon went, and its cell is closed, its last commits durable and
//! uploaded, as a host upgrading its daemon needs. Before, the daemon exited at once, its cell as
//! a crash leaves it: an ordered commit made just before not uploaded, nor perhaps durable.

// cladefs: the daemon's binary, on Linux and macOS (board 13065).
#![cfg(not(madsim))]

mod common;

use cladefs::{CellClient, ExtentId, OpenCell, VolatileSlice, VolatileSliceMut};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const BLOCK: usize = OpenCell::BLOCK_SIZE as usize;

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A daemon serving at `socket`, of placement `token`, keeping its cells in the bucket
/// `bucket`, a directory: what it uploads stays there once it exits, for another to read.
fn daemon(socket: &Path, bucket: &Path, token: u64) -> KillOnDrop {
    let daemon = Command::new(env!("CARGO_BIN_EXE_cladefsd"))
        .arg(socket)
        .env("CLADEFSD_ROOTS", socket.parent().unwrap())
        .env("CLADEFSD_BUCKET", format!("file://{}", bucket.display()))
        .env("CLADEFSD_TOKEN", token.to_string())
        .spawn()
        .unwrap();
    let daemon = KillOnDrop(daemon);
    let deadline = Instant::now() + Duration::from_secs(30);
    while std::os::unix::net::UnixStream::connect(socket).is_err() {
        assert!(Instant::now() < deadline, "the daemon never served");
        std::thread::sleep(Duration::from_millis(20));
    }
    daemon
}

fn read(client: &CellClient, extent: ExtentId) -> Vec<u8> {
    let mut data = vec![0; BLOCK];
    client.read(extent, 0, &mut [VolatileSliceMut::from(&mut data[..])]).unwrap();
    data
}

#[tokio::test(flavor = "multi_thread")]
async fn sigterm_closes_every_cell_durable_and_uploaded() {
    let dir = tempfile::tempdir().unwrap();
    let bucket = dir.path().join("bucket");
    std::fs::create_dir(&bucket).unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let mut first = daemon(&socket, &bucket, 1);

    // A VM's cell, its last commit only ordered, which its uploader, uploading within a second of
    // a durable commit, has not uploaded as the daemon is told to stop.
    let client = CellClient::connect_new(&socket, 0, 1 << 28).unwrap();
    let (told, lost) = std::sync::mpsc::channel();
    client.on_daemon_lost(move || told.send(()).unwrap());
    let cell = common::cell_of(&client);
    let path = common::cell_dir(dir.path(), cell);
    let extent = ExtentId { cell: cell.id, sequence: 1 };
    client.write(extent, 0, &[VolatileSlice::from(&[7; BLOCK][..])]).unwrap();
    client.commit_ordered(Some(b"drained")).unwrap();

    // SAFETY: Signals the daemon this test started.
    assert_eq!(unsafe { libc::kill(first.0.id() as i32, libc::SIGTERM) }, 0);
    let status = first.0.wait().unwrap();
    assert!(status.success(), "{status}");
    assert_eq!(lost.recv_timeout(Duration::from_secs(10)), Ok(()), "the VM process wasn't told");
    drop(client);

    // Closed, durable: as of its last commit here.
    let closed = OpenCell::open(&path).await.unwrap();
    assert_eq!(closed.root().unwrap(), b"drained");
    drop(closed);

    // Uploaded: a later placement's daemon, on another host, with nothing of it, restores a
    // VM of the cell from the bucket, its commit there.
    let other = dir.path().join("other");
    std::fs::create_dir(&other).unwrap();
    let second_socket = other.join("cladefsd.sock");
    let _second = daemon(&second_socket, &bucket, 2);
    // TASK-364: its VM manager makes the cell there first, where the daemon owning that disk
    // keeps it, taken from the bucket.
    let taken: PathBuf = common::cell_dir(&other, cell);
    std::fs::create_dir_all(taken.parent().unwrap()).unwrap();
    let full = host_fs::may_block(|| local_cell::host_cell(&path).unwrap());
    // SAFETY: The test's own process, whose other threads read no environment meanwhile.
    unsafe { std::env::set_var("CLADEFSD_BUCKET", format!("file://{}", bucket.display())) };
    cladefs::Opening::here()
        .unwrap()
        .take(&taken, 1 << 28, full, Some(2))
        .await
        .unwrap()
        .close()
        .await;
    let taken = CellClient::connect_cell(&second_socket, cell).unwrap();
    assert_eq!(taken.root().unwrap(), b"drained");
    assert_eq!(read(&taken, extent), [7; BLOCK]);
}

/// board 8503: its health reporter gone once it reported SERVING, a drain waits for the health
/// service to report NOT_SERVING no longer than `drain::REPORTING`: it drains all the same, and
/// exits 1, saying why, within its bound. Before, it waited for ever, outside its bound, the
/// daemon neither drained nor exited.
#[tokio::test(flavor = "multi_thread")]
async fn drains_whose_health_reporter_went_fail_within_their_bound() {
    use tonic_health::pb::HealthCheckRequest;
    use tonic_health::pb::health_check_response::ServingStatus;
    use tonic_health::pb::health_client::HealthClient;
    let dir = tempfile::tempdir().unwrap();
    let bucket = dir.path().join("bucket");
    std::fs::create_dir(&bucket).unwrap();
    let (socket, control) = (dir.path().join("cladefsd.sock"), dir.path().join("control.sock"));
    let log = dir.path().join("log");
    let bound = Duration::from_secs(10);
    let daemon = Command::new(env!("CARGO_BIN_EXE_cladefsd"))
        .arg(&socket)
        .env("CLADEFSD_ROOTS", dir.path())
        .env("CLADEFSD_BUCKET", format!("file://{}", bucket.display()))
        .env("CLADEFSD_TOKEN", "1")
        .env("CLADEFSD_GRPC", &control)
        .env("CLADEFSD_DRAIN", bound.as_secs().to_string())
        .env("FAILPOINTS", "cladefsd::health_reporter_gone=return")
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let mut daemon = KillOnDrop(daemon);
    let said = || std::fs::read_to_string(&log).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while std::os::unix::net::UnixStream::connect(&socket).is_err() {
        assert!(Instant::now() < deadline, "the daemon never served: {}", said());
        std::thread::sleep(Duration::from_millis(20));
    }
    // The control API's status SERVING, reported last, after which its reporter goes.
    let mut health =
        HealthClient::new(cladefs::cladefsd::control::channel(&control).await.unwrap());
    let api = HealthCheckRequest { service: "cladefs.cladefsd.v1.Cladefsd".into() };
    while health.check(api.clone()).await.unwrap().into_inner().status() != ServingStatus::Serving {
        assert!(Instant::now() < deadline, "never SERVING: {}", said());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // SAFETY: Signals the daemon this test started.
    assert_eq!(unsafe { libc::kill(daemon.0.id() as i32, libc::SIGTERM) }, 0);
    let signalled = Instant::now();
    let status = loop {
        if let Some(status) = daemon.0.try_wait().unwrap() {
            break status;
        }
        assert!(signalled.elapsed() < bound, "not exited within the drain's bound: {}", said());
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(1), "{}", said());
    assert!(said().contains("the health service never reported NOT_SERVING"), "{}", said());
}
