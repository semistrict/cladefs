// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A cell a daemon served is whole on its disk once its VM process goes, as a host's check of a
//! guest's crash finds it: as of its last commit, durable or only ordered, as a guest's fsyncs are
//! (TASK-26.6.2). The daemon closes the cell as the VM process goes, flushing it. cladefs: a host's
//! one daemon owns its cells, which no other process opens beside it (board 14305): so the check
//! waits for that close as a writer does, by opening the cell again through the daemon, then kills
//! the daemon, as a crash would, no drain flushing anything, and opens the cell itself.

// cladefs: the daemon's, on Linux and macOS (board 13065).
#![cfg(not(madsim))]

mod common;

use cladefs::{CellClient, CellId, ExtentId, OpenCell, VolatileSlice, VolatileSliceMut};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A daemon serving in `dir`, and its socket: its cells in a file bucket there, which
/// another process reads what they hold in once it has left their directories, uploaded
/// (TASK-165.8).
fn daemon(dir: &Path) -> (KillOnDrop, PathBuf) {
    let socket = dir.join("cladefsd.sock");
    let bucket = format!("file://{}/", dir.canonicalize().unwrap().join("bucket").display());
    let daemon = Command::new(env!("CARGO_BIN_EXE_cladefsd"))
        .arg(&socket)
        .env("CLADEFSD_ROOTS", dir)
        .env("CLADEFSD_BUCKET", bucket)
        .env("CLADEFSD_TOKEN", "1")
        .spawn()
        .unwrap();
    let daemon = KillOnDrop(daemon);
    let deadline = Instant::now() + Duration::from_secs(30);
    while std::os::unix::net::UnixStream::connect(&socket).is_err() {
        assert!(Instant::now() < deadline, "the daemon never served");
        std::thread::sleep(Duration::from_millis(20));
    }
    (daemon, socket)
}

/// The cell `cell`, closed by `daemon` as its VM process went, then `daemon` killed: its VM's
/// next connection waiting for that close (the cell table's `writers_wait_for_closing`), the
/// daemon then dying, as a crash kills it, nothing flushed past what its close did; and the
/// daemon started again for the next round.
fn closed_then_crashed(dir: &Path, daemon: &mut (KillOnDrop, PathBuf), cell: CellId) {
    let waited = CellClient::connect_cell(&daemon.1, cell).unwrap();
    daemon.0.0.kill().unwrap();
    daemon.0.0.wait().unwrap();
    drop(waited);
    std::fs::remove_file(&daemon.1).unwrap();
    *daemon = self::daemon(dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn cells_open_whole_in_another_process_once_their_vm_goes() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = daemon(dir.path());
    let cell = common::cell_of(&CellClient::connect_new(&daemon.1, 0, 1 << 28).unwrap());
    let path = common::cell_dir(dir.path(), cell);
    let mut sequence = 1;
    for round in 0..50 {
        let client = CellClient::connect_cell(&daemon.1, cell).unwrap();
        let mut root = Vec::new();
        for commit in 0..20 {
            for _ in 0..4 {
                let extent = ExtentId { cell: client.cell(), sequence };
                sequence += 1;
                let data = vec![(round * 20 + commit) as u8; 3 * 4096];
                client.write(extent, 0, &[VolatileSlice::from(&data[..])]).unwrap();
            }
            root = format!("round {round} commit {commit}").into_bytes();
            client.commit(Some(&root)).unwrap();
        }
        drop(client);
        closed_then_crashed(dir.path(), &mut daemon, cell);
        let opened = OpenCell::open(&path).await.unwrap();
        assert_eq!(opened.root().unwrap(), root, "round {round}");
        sequence = opened.info().unwrap().next_sequence.max(sequence);
        opened.close().await;
    }
}

/// A guest's fsyncs are ordered commits, which its host makes durable within its lag: one whose VM
/// goes right after them opens in another process with every one of them, as the daemon makes
/// them durable before letting the cell go. The crash test's lost fsyncs (TASK-26.6.2) were its
/// check opening the cell before then, which recovered it as of the daemon's last flush.
#[tokio::test(flavor = "multi_thread")]
async fn ordered_commits_are_whole_in_another_process_once_their_vm_goes() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = daemon(dir.path());
    let cell = common::cell_of(&CellClient::connect_new(&daemon.1, 0, 1 << 28).unwrap());
    let path = common::cell_dir(dir.path(), cell);
    let mut sequence = 1;
    for round in 0..50u64 {
        let client = CellClient::connect_cell(&daemon.1, cell).unwrap();
        let mut last = Vec::new();
        for commit in 0..20u64 {
            let extent = ExtentId { cell: client.cell(), sequence };
            sequence += 1;
            let data = vec![(round * 20 + commit) as u8; 4096];
            client.write(extent, 0, &[VolatileSlice::from(&data[..])]).unwrap();
            client.commit_ordered(None).unwrap();
            last.push((extent, data));
        }
        drop(client);
        closed_then_crashed(dir.path(), &mut daemon, cell);
        let opened = OpenCell::open(&path).await.unwrap();
        for (extent, data) in &last {
            let mut read = vec![0; data.len()];
            let found = opened.read(*extent, 0, &mut [VolatileSliceMut::from(&mut read[..])]).await;
            assert_eq!(found.map(|()| read), Ok(data.clone()), "round {round}: {extent:?}");
        }
        sequence = opened.info().unwrap().next_sequence.max(sequence);
        opened.close().await;
    }
}

/// TASK-50.2: a cell whose daemon goes is told at once, and what it asks fails from then on; one
/// that closes its connection itself is told nothing.
#[test]
fn cells_are_told_when_their_daemon_goes() {
    let dir = tempfile::tempdir().unwrap();
    let (mut daemon, socket) = daemon(dir.path());
    let closed = CellClient::connect_new(&socket, 0, 1 << 28).unwrap();
    let (told, lost) = std::sync::mpsc::channel();
    let told_closed = told.clone();
    closed.on_daemon_lost(move || told_closed.send("closed").unwrap());
    drop(closed);

    let client = CellClient::connect_new(&socket, 0, 1 << 28).unwrap();
    client.on_daemon_lost(move || told.send("cell").unwrap());
    let write = |sequence| {
        let extent = ExtentId { cell: client.cell(), sequence };
        client.write(extent, 0, &[VolatileSlice::from(&[7; 4096][..])])
    };
    write(1).unwrap();
    daemon.0.kill().unwrap();
    daemon.0.wait().unwrap();
    assert_eq!(lost.recv_timeout(Duration::from_secs(10)), Ok("cell"));
    assert_eq!(write(2), Err(cladefs::Errno::EIO));
    assert!(lost.try_recv().is_err(), "a cell closed was told");
}
