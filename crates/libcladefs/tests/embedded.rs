// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! libcladefs: a daemon embedded in this process (board 13471) serves cells to a device of this
//! process's, and to VM processes it starts, over a socket pair it hands them and over a socket,
//! on Linux and on macOS (board 13065). One test: a process has one daemon.

#![cfg(not(madsim))]

use cladefs_core::{CellId, Errno, ExtentId, VolatileSlice, VolatileSliceMut};
use libcladefs::{CellClient, Config, Daemon, DaemonConnection, Kind};
use std::os::fd::{FromRawFd, IntoRawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// A cell's bytes.
const CAPACITY: u64 = 1 << 26;

/// An extent's bytes.
const EXTENT: usize = 1 << 20;

/// The variable that makes this test's binary a VM process ([`vm_process`]): `socket:PATH` or
/// `fd:N`, then `,TENANT,CELL,SEQUENCE`.
const VM_PROCESS: &str = "LIBCLADEFS_TEST_VM_PROCESS";

/// The extent `sequence` of `cell`, its bytes each `sequence`'s.
fn extent(cell: u64, sequence: u64) -> (ExtentId, Vec<u8>) {
    (ExtentId { cell, sequence }, vec![sequence as u8; EXTENT])
}

/// Writes extent `sequence` of `client`'s cell and commits it.
fn write(client: &CellClient, sequence: u64) {
    let (extent, bytes) = extent(client.cell(), sequence);
    client.write(extent, 0, &[VolatileSlice::from(&bytes[..])]).unwrap();
    client.commit(None).unwrap();
}

/// Reads extent `sequence` of `client`'s cell back, as [`write`] wrote it.
fn read_back(client: &CellClient, sequence: u64) {
    let (extent, written) = extent(client.cell(), sequence);
    let mut read = vec![0; EXTENT];
    client.read(extent, 0, &mut [VolatileSliceMut::from(&mut read[..])]).unwrap();
    assert!(read == written, "extent {sequence} read back other than written");
}

/// This test's binary started as a VM process of the daemon's, as `how` says to reach it.
fn vm_process(how: &str) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "vm_process_role", "--nocapture"])
        .env(VM_PROCESS, how)
        .spawn()
        .unwrap()
}

/// A VM process's role, when [`VM_PROCESS`] says so: opens the cell it names, reads what the
/// extent before `SEQUENCE` holds, writes and commits extent `SEQUENCE`, and reads it back.
#[test]
fn vm_process_role() {
    let Ok(how) = std::env::var(VM_PROCESS) else { return };
    let mut parts = how.split(',');
    let mut next = || parts.next().unwrap();
    let (reach, tenant, id, sequence) = (next(), next(), next(), next());
    let cell = CellId { tenant: tenant.parse().unwrap(), id: id.parse().unwrap() };
    let sequence: u64 = sequence.parse().unwrap();
    let connection = match reach.split_once(':').unwrap() {
        ("socket", path) => DaemonConnection::connect(path).unwrap(),
        // SAFETY: The descriptor the embedding process left open for this one, owned here.
        ("fd", fd) => {
            DaemonConnection::over(unsafe { UnixStream::from_raw_fd(fd.parse().unwrap()) }).unwrap()
        }
        (other, _) => panic!("no way of reaching the daemon: {other}"),
    };
    let client = connection.open_cell(cell).unwrap();
    read_back(&client, sequence - 1);
    write(&client, sequence);
    read_back(&client, sequence);
}

#[test]
fn embedded_daemon_serves_cells_in_process_and_to_vm_processes() {
    if std::env::var_os(VM_PROCESS).is_some() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let config = Config::local(dir.path()).memory(256 << 20);
    let daemon = futures::executor::block_on(Daemon::start(config)).unwrap();
    assert!(Daemon::new().is_err(), "a second daemon in one process");

    // A device of this process's: a cell made, written, committed, closed, and opened again.
    let made = daemon.create(1, CAPACITY).unwrap();
    let cell = CellId { tenant: 1, id: made.cell() };
    write(&made, 1);
    read_back(&made, 1);
    drop(made);
    let opened = daemon.open(cell).unwrap();
    read_back(&opened, 1);
    drop(opened);

    // A VM process this process started, over a socket pair it handed it.
    // Served once started, as its child, not reaped until it is done.
    let (ours, theirs) = UnixStream::pair().unwrap();
    let theirs = theirs.into_raw_fd();
    // SAFETY: Clears close-on-exec of a descriptor this process owns, which the child inherits.
    assert_eq!(unsafe { libc_clear_cloexec(theirs) }, 0);
    let mut child = vm_process(&format!("fd:{theirs},{},{},2", cell.tenant, cell.id));
    // SAFETY: This process's copy of the child's end, which only the child uses.
    drop(unsafe { std::os::fd::OwnedFd::from_raw_fd(theirs) });
    daemon.serve_vm_process(ours, child.id()).unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "the VM process over a socket pair: {status}");
    read_back(&daemon.open(cell).unwrap(), 2);

    // A VM process connecting to the daemon's socket.
    let socket = dir.path().join("vms.sock");
    daemon.serve_vm_processes(&socket).unwrap();
    let how = format!("socket:{},{},{},3", socket.display(), cell.tenant, cell.id);
    let status = vm_process(&how).wait().unwrap();
    assert!(status.success(), "the VM process over a socket: {status}");
    read_back(&daemon.open(cell).unwrap(), 3);

    // Its control API, for an endpoint of this process's own.
    futures::executor::block_on(daemon.control_service()).unwrap();
    assert!(Path::new(&dir.path().join("tenants")).is_dir());

    // A cell embedded in this process, held, holds its drain until let go, and a daemon draining
    // embeds none (board 13909; Codex, hotbed review 162: the drain timed out on its uploader,
    // and embedded more).
    let embedded =
        futures::executor::block_on(daemon.create_embedded(1, CAPACITY, Kind::Sqlite)).unwrap();
    let held = futures::executor::block_on(daemon.clone().stop(Duration::from_secs(2)));
    let held = held.unwrap_err().to_string();
    assert!(held.contains("1 embedded cells"), "{held}");
    let refused = futures::executor::block_on(daemon.create_embedded(1, CAPACITY, Kind::Sqlite));
    assert_eq!(refused.map(drop).unwrap_err().errno(), Some(Errno::EBUSY));
    drop(embedded);

    futures::executor::block_on(daemon.stop(Duration::from_secs(30))).unwrap();
}

/// A boot refused, its configuration naming no disk, returns its error to a caller awaiting it on
/// its own tokio runtime, which drops none of the daemon's (Codex, hotbed review 151): the process's
/// daemon is kept, and no other made.
#[tokio::test(flavor = "multi_thread")]
async fn a_boot_refused_returns_its_error_on_the_callers_runtime() {
    if std::env::var_os(VM_PROCESS).is_some() {
        return;
    }
    let refused = Daemon::start(Config::from_toml("").unwrap()).await.map(drop);
    let refused = refused.unwrap_err().to_string();
    assert!(refused.contains("[[disks]]"), "{refused}");
    assert!(Daemon::new().is_err(), "a second daemon in one process");
}

/// Clears `fd`'s close-on-exec flag.
unsafe fn libc_clear_cloexec(fd: i32) -> i32 {
    // SAFETY: As the caller promises: a descriptor it owns.
    unsafe { libc::fcntl(fd, libc::F_SETFD, 0) }
}
