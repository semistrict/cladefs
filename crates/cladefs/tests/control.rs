// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-43.3: the extent daemon's control API, gRPC over a Unix socket, as the host's VM manager
//! calls it, from a client in Rust: every call, and the errors a cell a VM has open gives.
//!
//! The daemon serves on threads of this process: its control socket, and a VM socket, whose
//! connections, VM processes here, have cells open.

// cladefs: the daemon's, on Linux and macOS (board 13065).
#![cfg(not(madsim))]

mod common;

use cladefs::cladefsd::control;
use cladefs::cladefsd::control::proto::*;
use cladefs::{CellClient, CellId, Errno};
use common::daemon::{CAPACITY, Daemon, NEW, failure, name, write};
use common::pattern;
use std::time::Duration;
use tonic::Code;

/// A cell made, written by a VM, grown, forked, made a public base, synced to object storage,
/// dropped locally and taken back from object storage, placed, and deleted, through the API.
#[test]
fn cells_through_the_control_api() {
    // SAFETY: nextest runs each test in a process of its own, whose other threads don't read the
    // environment yet.
    unsafe {
        std::env::set_var("CLADEFSD_BUCKET", "memory:///cells");
        std::env::set_var("CLADEFSD_TOKEN", "1");
    }
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());

    let cell = daemon.create(NEW, None).unwrap();
    assert_ne!(cell.id, 0);
    let vm = daemon.open(cell);
    assert_eq!(vm.cell(), cell.id);
    write(&vm, &pattern(1), b"base");
    drop(vm);
    let stat = daemon.wait_closed(cell);
    assert_eq!(
        stat,
        StatCellResponse {
            tenant: cell.tenant,
            cell: cell.id,
            capacity: CAPACITY,
            used: 64 << 10,
            available: CAPACITY - (64 << 10),
            public: false,
            open: false,
            own: 64 << 10,
            inherited: 0,
            own_extents: 1,
            inherited_extents: 0,
            committed: 1,
            // Closed, the stat opens it, as the daemon opens any cell, taking its cell's head
            // after its making's and its VM's (TASK-165.9): what it committed, uploaded.
            uploading: true,
            taken_token: 1,
            taken_epoch: 3,
            taken_unix_ms: stat.taken_unix_ms,
            uploaded: 1,
            pending_upload_bytes: 0,
            oldest_pending_ms: 0,
            fsync: FsyncLevel::Ordered.into(),
            // Not using Fast WAL (doc-29).
            fast_wal: false,
            fast_wal_bucket: String::new(),
            fast_wal_degraded: false,
            ephemeral: false,
            // Never placed: the daemon's placement's, CLADEFSD_TOKEN (board 13879).
            placement_token: 1,
        }
    );

    daemon.resize(cell, 2 * CAPACITY).unwrap();
    assert_eq!(daemon.stat(cell).unwrap().capacity, 2 * CAPACITY);
    // Its guest, which would have to agree, isn't there.
    assert_eq!(failure(daemon.resize(cell, CAPACITY)), (Code::FailedPrecondition, Errno::EBUSY));

    let forked = daemon.fork(cell, None).unwrap();
    assert_ne!(forked, cell);
    let stat = daemon.stat(forked).unwrap();
    assert_eq!((stat.cell, stat.used, stat.public), (forked.id, 64 << 10, false));
    // TASK-49: all it holds, it inherited; TASK-454: the extent too.
    assert_eq!((stat.own, stat.inherited), (0, 64 << 10));
    assert_eq!((stat.own_extents, stat.inherited_extents), (0, 1));
    daemon.make_public(cell).unwrap();
    assert!(daemon.stat(cell).unwrap().public);
    assert!(!daemon.stat(forked).unwrap().public, "a fork of a public base is its own");

    // The fork's commits in object storage, dropped locally and taken again from object storage,
    // placed after this one's: what it holds there.
    daemon.sync(forked).unwrap();
    daemon.drop_cell(forked).unwrap();
    assert_eq!(daemon.create(forked, Some(2)).unwrap(), forked);
    let vm = daemon.open(forked);
    assert_eq!(vm.root().unwrap(), b"base");
    drop(vm);
    // Board 13879: closed, the placement CreateCell made it at.
    assert_eq!(daemon.wait_closed(forked).placement_token, 2);

    daemon.place(cell, 3).unwrap();
    let placed = daemon.stat(cell).unwrap();
    assert_eq!((placed.open, placed.placement_token), (false, 3));
    daemon.delete(forked).unwrap();
    assert_eq!(failure(daemon.stat(forked)), (Code::NotFound, Errno::ENOENT));

    let counters = daemon.counters();
    let object_storage = counters.object_storage.expect("the daemon has a bucket");
    assert!(object_storage.requests["create"] > 0, "{object_storage:?}");
    assert!(object_storage.uploaded_bytes.values().sum::<u64>() >= 64 << 10, "{object_storage:?}");
}

/// TASK-50.5: the cells VM processes have open are listed, with their writers' takings of their
/// cells' heads, as this host's placement took them, and their uploads: synced, nothing waits.
#[test]
fn open_cells_are_listed_with_their_takings_and_uploads() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let before = std::time::SystemTime::now();
    let (first, one) = daemon.open_new(0, CAPACITY);
    write(&one, &pattern(1), b"one");
    let (second, two) = daemon.open_new(0, CAPACITY);
    daemon.sync(first).unwrap();
    daemon.sync(second).unwrap();
    let after = std::time::SystemTime::now();

    let listed = daemon.list();
    let mut expected = [(first, 1u64), (second, 0u64)];
    expected.sort_by_key(|(id, _)| (id.tenant, id.id));
    let ids: Vec<_> = listed.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, [expected[0].0, expected[1].0]);
    let millis = |time: std::time::SystemTime| {
        time.duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
    };
    for ((_, stat), (cell, extents)) in listed.iter().zip(expected) {
        assert_eq!((stat.cell, stat.open, stat.uploading), (cell.id, true, true));
        // TASK-454: the extent the first wrote; none, the second.
        assert_eq!((stat.own_extents, stat.inherited_extents), (extents, 0));
        assert_eq!((stat.taken_token, stat.taken_epoch, stat.placement_token), (1, 1, 1));
        assert!((millis(before)..=millis(after)).contains(&stat.taken_unix_ms), "{stat:?}");
        assert_eq!(stat.uploaded, stat.committed, "{stat:?}");
        assert_eq!((stat.pending_upload_bytes, stat.oldest_pending_ms), (0, 0));
    }
    assert_eq!(daemon.stat(expected[0].0).unwrap(), listed[0].1);
    drop((one, two));
}

/// TASK-454: StatCell and ListCells say how many extents a cell holds, its own and those it
/// inherited, as its VM writes and frees them, open and closed, and of its fork.
#[test]
fn extents_are_counted_through_the_control_api() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let write = |vm: &CellClient, sequence: u64| {
        let extent = cladefs::ExtentId { cell: vm.cell(), sequence };
        vm.write(extent, 0, &[cladefs::VolatileSlice::from(&pattern(sequence)[..4096])]).unwrap();
        extent
    };
    let counts = |stat: &StatCellResponse| (stat.own_extents, stat.inherited_extents);

    let (base, vm) = daemon.open_new(0, CAPACITY);
    assert_eq!(counts(&daemon.stat(base).unwrap()), (0, 0));
    let written: Vec<_> = (1..=3).map(|sequence| write(&vm, sequence)).collect();
    vm.commit(None).unwrap();
    assert_eq!(counts(&daemon.stat(base).unwrap()), (3, 0));
    drop(vm);
    assert_eq!(counts(&daemon.stat(base).unwrap()), (3, 0));

    let fork = daemon.fork(base, None).unwrap();
    assert_eq!(counts(&daemon.stat(fork).unwrap()), (0, 3));
    let vm = daemon.open(fork);
    let own = [write(&vm, 1), write(&vm, 2)];
    vm.free(written[0]).unwrap();
    vm.free(own[1]).unwrap();
    // Open, as its VM has it, committed or not.
    assert_eq!(counts(&daemon.stat(fork).unwrap()), (1, 2));
    vm.commit(None).unwrap();
    let listed = daemon.list().into_iter().find(|(id, _)| *id == fork);
    assert_eq!(listed.map(|(_, stat)| counts(&stat)), Some((1, 2)));
    drop(vm);
    assert_eq!(counts(&daemon.stat(fork).unwrap()), (1, 2));
    assert_eq!(counts(&daemon.stat(base).unwrap()), (3, 0));
}

/// A cell a VM process has open is its: the API refuses what would change it under its guest,
/// grows it, forks it live, and syncs it; and calls fail with the errnos the .proto says.
#[test]
fn open_cells_are_their_vms() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let (cell, vm) = daemon.open_new(0, CAPACITY);
    write(&vm, &pattern(2), b"open");

    assert!(daemon.stat(cell).unwrap().open);
    let busy = (Code::FailedPrecondition, Errno::EBUSY);
    assert_eq!(failure(daemon.delete(cell)), busy);
    assert_eq!(failure(daemon.make_public(cell)), busy);
    assert_eq!(failure(daemon.place(cell, 1)), busy);
    assert_eq!(failure(daemon.create(cell, None)), (Code::AlreadyExists, Errno::EEXIST));
    // Grown at once, as its VM process sees.
    daemon.resize(cell, 2 * CAPACITY).unwrap();
    assert_eq!(vm.info().unwrap().capacity, 2 * CAPACITY);
    // Forked live: nothing is open on it.
    let forked = daemon.fork(cell, None).unwrap();
    assert_eq!(daemon.stat(forked).unwrap().cell, forked.id);

    assert_eq!(failure(daemon.create(forked, None)), (Code::AlreadyExists, Errno::EEXIST));
    let none = CellId { tenant: 0, id: 0xdead_beef };
    assert_eq!(failure(daemon.stat(none)), (Code::NotFound, Errno::ENOENT));
    assert_eq!(failure(daemon.stat(NEW)), (Code::InvalidArgument, Errno::EINVAL));
    // Synced as its VM process goes on.
    daemon.sync(cell).unwrap();

    // Gone with its VM process, once the daemon finds it gone, it is the API's.
    drop(vm);
    daemon.wait_closed(cell);
    daemon.delete(cell).unwrap();
}

/// A container image imported is a cell holding its files.
#[test]
fn imports_images() {
    use cladefs::import::testing::{Entry, Kind, layer};
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let image = dir.path().join("layer.tar");
    let hello = b"hello from the image\n";
    std::fs::write(&image, layer(&[Entry::new("hello", Kind::File(hello))]).unwrap()).unwrap();

    let cell = daemon.import(0, &image).unwrap();
    assert_eq!(daemon.stat(cell).unwrap().cell, cell.id);
    let cell_dir = daemon.cell_path(cell);

    let size = futures::executor::block_on(async {
        let fs = cladefs::Filesystem::mount(cladefs::Storage::open(&cell_dir).await.unwrap())
            .await
            .unwrap();
        let (_, attr) = fs.root().lookup(b"hello").await.unwrap();
        fs.close().await.unwrap();
        attr.size
    });
    assert_eq!(size, hello.len() as u64);
}

/// An image that cannot be made into a cell leaves none behind: one whose entry's time is out of
/// range panicked the import, which left the cell half made.
#[test]
fn imports_that_fail_leave_no_cell() {
    use cladefs::import::testing::{Entry, Kind, layer};
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let image = |name: &str, entry: Entry<'_>| {
        let path = dir.path().join(name);
        std::fs::write(&path, layer(&[entry]).unwrap()).unwrap();
        path
    };
    let out_of_range = image("out-of-range.tar", Entry::new("f", Kind::File(b"f")).mtime(u64::MAX));
    let whole = image("whole.tar", Entry::new("f", Kind::File(b"f")));

    assert_eq!(failure(daemon.import(0, &out_of_range)), (Code::InvalidArgument, Errno::EINVAL));
    let tenant0 = dir.path().join("disk0/cells").join(base32_id::Padded(0).to_string());
    if tenant0.exists() {
        assert_eq!(std::fs::read_dir(&tenant0).unwrap().count(), 0, "left half-made cell");
    }
    let cell = daemon.import(0, &whole).unwrap();
    assert_eq!(daemon.stat(cell).unwrap().cell, cell.id);
}

/// An import its caller gave up on runs to its end cleanly (board 5599, board 5384).
#[test]
fn imports_given_up_on_hold_their_paths_to_their_end() {
    use cladefs::import::testing::{Entry, Kind, layer};
    use std::sync::{Mutex, mpsc};
    let dir = tempfile::tempdir().unwrap();
    let daemon = Daemon::new(dir.path());
    let image = dir.path().join("layer.tar");
    let hello = b"hello from the image\n";
    std::fs::write(&image, layer(&[Entry::new("hello", Kind::File(hello))]).unwrap()).unwrap();

    // The import held under way until let go.
    let (under_way, seen) = mpsc::channel();
    let (go, gone) = mpsc::channel::<()>();
    let gone = Mutex::new(gone);
    fail::cfg_callback("cladefsd::import", move || {
        under_way.send(()).unwrap();
        gone.lock().unwrap().recv().unwrap();
    })
    .unwrap();
    // Its caller gives up once it is under way: the call dropped, its stream reset.
    let request = ImportImageRequest {
        capacity: CAPACITY,
        image: name(&image),
        reference: String::new(),
        tenant: 0,
    };
    let mut client = daemon.client.clone();
    daemon.runtime.block_on(async {
        let call = tokio::spawn(async move { client.import_image(request).await });
        tokio::task::spawn_blocking(move || seen.recv().unwrap()).await.unwrap();
        call.abort();
        assert!(call.await.unwrap_err().is_cancelled());
    });

    // Let go, the import ends and publishes the completed cell into disk0/cells/<tenant0>/<cell>.
    fail::remove("cladefsd::import");
    go.send(()).unwrap();
    let tenant0 = dir.path().join("disk0/cells").join(base32_id::Padded(0).to_string());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let cell_dir = loop {
        let found = std::fs::read_dir(&tenant0).ok().and_then(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .find(|p| p.is_dir() && !p.file_name().unwrap().to_string_lossy().starts_with('.'))
        });
        if let Some(found) = found {
            break found;
        }
        assert!(std::time::Instant::now() < deadline, "aborted import never finished");
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let size = futures::executor::block_on(async {
        let fs = cladefs::Filesystem::mount(cladefs::Storage::open(&cell_dir).await.unwrap())
            .await
            .unwrap();
        let (_, attr) = fs.root().lookup(b"hello").await.unwrap();
        fs.close().await.unwrap();
        attr.size
    });
    assert_eq!(size, hello.len() as u64);
}

/// A fork of a cell this host never had: the cell taken from object storage
/// (CreateCell of its cell, placed after the host that wrote it), then forked (ForkCell), whose
/// VM reads what the cell held, its data fetched as read.
#[test]
fn forks_of_cells_never_had_here() {
    // SAFETY: as in cells_through_the_control_api.
    unsafe {
        std::env::set_var("CLADEFSD_BUCKET", "memory:///elsewhere");
        std::env::set_var("CLADEFSD_TOKEN", "1");
    }
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    // Another host's: written, synced, and gone from here.
    let (cell, vm) = daemon.open_new(0, CAPACITY);
    write(&vm, &pattern(7), b"elsewhere");
    drop(vm);
    daemon.wait_closed(cell);
    daemon.sync(cell).unwrap();
    daemon.drop_cell(cell).unwrap();

    assert_eq!(daemon.create(cell, Some(2)).unwrap(), cell);
    let forked = daemon.fork(cell, None).unwrap();
    let vm = daemon.open(forked);
    assert_eq!(vm.cell(), forked.id);
    assert_eq!(vm.root().unwrap(), b"elsewhere");
    let extent = cladefs::ExtentId { cell: cell.id, sequence: 1 };
    let mut read = vec![0; 64 << 10];
    vm.read(extent, 0, &mut [cladefs::VolatileSliceMut::from(&mut read[..])]).unwrap();
    assert!(read == pattern(7), "the fork reads other bytes");
}

/// TASK-43.6: calls waiting hold no thread: shrinks waiting for guests that don't answer, more of
/// them than the threads the calls' I/O runs on, leave another call to complete at once, and end
/// as the guests' time to answer does.
#[test]
fn shrinks_waiting_for_their_guests_hold_no_threads() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    // Raise RLIMIT_NOFILE (e.g. on macOS, whose default 10,240 admits fewer than
    // BLOCKING_THREADS + 16 connections under `cell_open_files()`).
    cladefs::cladefsd::raise_open_files_limit().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let idle = daemon.create(NEW, None).unwrap();
    let count = cladefs::cladefsd::control::BLOCKING_THREADS + 16;
    let asked = Arc::new(AtomicUsize::new(0));
    let (vms, cells): (Vec<_>, Vec<_>) = (0..count)
        .map(|_| {
            let (cell, client) = daemon.open_new(0, CAPACITY);
            // A guest that never answers.
            let asked = asked.clone();
            client.on_capacity(move |_, ask| {
                if ask {
                    asked.fetch_add(1, Ordering::Relaxed);
                }
            });
            (client, cell)
        })
        .collect();

    let shrinks: Vec<_> = (cells.iter())
        .map(|&cell| {
            let mut client = daemon.client.clone();
            let request =
                ResizeCellRequest { tenant: cell.tenant, cell: cell.id, capacity: CAPACITY / 2 };
            daemon.runtime.spawn(async move { client.resize_cell(request).await })
        })
        .collect();
    daemon.runtime.block_on(async {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while asked.load(Ordering::Relaxed) < count {
            assert!(tokio::time::Instant::now() < deadline, "the guests weren't all asked");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    });
    let start = std::time::Instant::now();
    assert_eq!(daemon.stat(idle).unwrap().capacity, CAPACITY);
    assert!(start.elapsed() < std::time::Duration::from_secs(2), "{:?}", start.elapsed());

    for shrink in shrinks {
        let shrunk = daemon.runtime.block_on(shrink).unwrap();
        assert_eq!(failure(shrunk), (Code::DeadlineExceeded, Errno::ETIMEDOUT));
    }
    drop(vms);
}

/// TASK-48: the control socket serves the standard gRPC health service: NOT_SERVING until the
/// daemon accepts VM processes, SERVING while it does, NOT_SERVING once it stops; Watch streams
/// each change, for the whole server and the control API alike.
#[test]
fn health_follows_whether_the_daemon_serves() {
    use tonic_health::pb::HealthCheckRequest;
    use tonic_health::pb::health_check_response::ServingStatus;
    use tonic_health::pb::health_client::HealthClient;
    // SAFETY: As above.
    unsafe {
        std::env::set_var("CLADEFSD_BUCKET", "memory:///cells");
        std::env::set_var("CLADEFSD_TOKEN", "1");
    }
    let dir = tempfile::tempdir().unwrap();
    let control = dir.path().join("control.sock");
    let listener = std::os::unix::net::UnixListener::bind(&control).unwrap();
    let tenants = dir.path().join("tenants");
    std::thread::spawn(move || cladefs::cladefsd::serve_control(listener, tenants));
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        let channel = cladefs::cladefsd::control::channel(&control).await.unwrap();
        let health = HealthClient::new(channel);
        let mut watcher = health.clone();
        let status = |service: &str| {
            let mut health = health.clone();
            let request = HealthCheckRequest { service: service.to_owned() };
            async move { health.check(request).await.unwrap().into_inner().status() }
        };
        let api = "cladefs.cladefsd.v1.Cladefsd";
        assert_eq!(status("").await, ServingStatus::NotServing);
        assert_eq!(status(api).await, ServingStatus::NotServing);
        let request = HealthCheckRequest { service: String::new() };
        let mut watch = watcher.watch(request).await.unwrap().into_inner();
        let mut next = async || watch.message().await.unwrap().unwrap().status();
        assert_eq!(next().await, ServingStatus::NotServing);

        let vms = std::os::unix::net::UnixListener::bind(dir.path().join("vms.sock")).unwrap();
        std::thread::spawn(move || cladefs::cladefsd::serve(vms));
        assert_eq!(next().await, ServingStatus::Serving);
        assert_eq!(status(api).await, ServingStatus::Serving);

        cladefs::cladefsd::stop_serving().await;
        assert_eq!(next().await, ServingStatus::NotServing);
        assert_eq!(status(api).await, ServingStatus::NotServing);
    });
}

/// TASK-411.7, and TASK-411's AC #3 (cladefsd serving a VM's encrypted cell through its
/// socket): two cells of a tenant whose KEK the daemon's KMS (`CLADEFS_KMS`) keeps, each
/// written by its VM, read back, no file of their directories holding what they wrote in the
/// clear. `ShredCell` takes the first cell's wrapped KEK out of its object and halts it,
/// its VM's reads failing with EACCES, the other's reading on; called again, it finds the cell
/// shredded and halts nothing more. `ForgetTenantKeys`, once the tenant's KEK is destroyed in the
/// KMS, halts the other. Closed, neither cell opens again (PERMISSION_DENIED, EACCES).
#[test]
fn encrypted_cells_served_and_shredded_through_the_control_api() {
    let keys = tempfile::tempdir().unwrap();
    let kms_dir = keys.path().join("kms");
    let kms = extent_keys::kms::LocalKms::open(&kms_dir).unwrap();
    kms.create(1).unwrap();
    // SAFETY: nextest runs each test in a process of its own, whose other threads don't read the
    // environment yet.
    unsafe { std::env::set_var("CLADEFS_KMS", format!("local://{}", kms_dir.display())) };
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let of_tenant = CellId { tenant: 1, id: 0 };
    let cell = daemon.create(of_tenant, None).unwrap();
    let other = daemon.create(of_tenant, None).unwrap();
    assert_ne!(cell, other);
    let vms = [daemon.open(cell), daemon.open(other)];
    for (seed, vm) in (1..).zip(&vms) {
        write(vm, &pattern(seed), b"encrypted");
    }
    let read = |vm: &CellClient, seed: u64| {
        let mut data = vec![0; 64 << 10];
        let extent = cladefs::ExtentId { cell: vm.cell(), sequence: 1 };
        let done = vm.read(extent, 0, &mut [cladefs::VolatileSliceMut::from(&mut data[..])]);
        done.map(|()| data == pattern(seed))
    };
    assert_eq!((read(&vms[0], 1), read(&vms[1], 2)), (Ok(true), Ok(true)));
    daemon.sync(cell).unwrap();
    daemon.sync(other).unwrap();
    // Beside the daemon's cells, which work on behind its API (TASK-463).
    for (seed, id) in (1..).zip([cell, other]) {
        let path = daemon.cell_path(id);
        let held = cladefs::testing::files_holding_beside(&path, &[&pattern(seed)[..64]]);
        assert_eq!(held, Vec::<String>::new(), "files holding what their VM wrote");
    }

    let shred = |daemon: &mut Daemon| {
        let request = ShredCellRequest { tenant: 1, cell: cell.id };
        let shredded = daemon.runtime.block_on(daemon.client.shred_cell(request)).unwrap();
        let ShredCellResponse { keks_removed, forgotten } = shredded.into_inner();
        (keks_removed, forgotten.unwrap().stores)
    };
    assert_eq!(shred(&mut daemon), (1, 1));
    assert_eq!((read(&vms[0], 1), read(&vms[1], 2)), (Err(Errno::EACCES), Ok(true)));
    assert_eq!(shred(&mut daemon), (0, 0));

    kms.destroy(1).unwrap();
    let request = ForgetTenantKeysRequest { tenant: 1, why: "tenant 1: destroyed".into() };
    let forgotten = daemon.runtime.block_on(daemon.client.forget_tenant_keys(request)).unwrap();
    assert_eq!(forgotten.into_inner().forgotten.unwrap().stores, 1);
    assert_eq!((read(&vms[0], 1), read(&vms[1], 2)), (Err(Errno::EACCES), Err(Errno::EACCES)));

    // Once the daemon lets them go, which it does after their VM processes' connections went.
    drop(vms);
    let denied = (Code::PermissionDenied, Errno::EACCES);
    assert_eq!(failure(daemon.stat_once_closed(cell)), denied);
    assert_eq!(failure(daemon.stat_once_closed(other)), denied);
}

/// TASK-411.27: a daemon given a master key (`CLADEFS_MASTER_KEY`) and no `CLADEFS_KMS` encrypts
/// its tenants' cells under the bucket KMS, each tenant's KEK made in its bucket on first use; what
/// VMs wrote is in no file of the cells' directories in the clear. `ShredTenant` replaces the first
/// tenant's KEK object by a tombstone and halts its cell, its VM's reads failing with EACCES, the
/// other tenant's reading on; called again, it finds the tenant shredded and halts nothing more.
/// Closed, the cell opens no more (PERMISSION_DENIED, EACCES).
#[test]
fn tenants_encrypted_under_the_bucket_kms_and_shredded_through_the_control_api() {
    let master: String = (0..32).map(|byte| format!("{byte:02x}")).collect();
    // SAFETY: nextest runs each test in a process of its own, whose other threads don't read the
    // environment yet.
    unsafe {
        std::env::remove_var("CLADEFS_KMS");
        std::env::set_var("CLADEFS_MASTER_KEY", &master);
    }
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let of = |tenant| CellId { tenant, id: 0 };
    let ours = daemon.create(of(1), None).unwrap();
    let theirs = daemon.create(of(2), None).unwrap();
    let vms = [daemon.open(ours), daemon.open(theirs)];
    for (seed, vm) in (1..).zip(&vms) {
        write(vm, &pattern(seed), b"encrypted");
    }
    let read = |vm: &CellClient, seed: u64| {
        let mut data = vec![0; 64 << 10];
        let extent = cladefs::ExtentId { cell: vm.cell(), sequence: 1 };
        let done = vm.read(extent, 0, &mut [cladefs::VolatileSliceMut::from(&mut data[..])]);
        done.map(|()| data == pattern(seed))
    };
    assert_eq!((read(&vms[0], 1), read(&vms[1], 2)), (Ok(true), Ok(true)));
    daemon.sync(ours).unwrap();
    daemon.sync(theirs).unwrap();
    for (seed, id) in (1..).zip([ours, theirs]) {
        let mut paths = vec![daemon.cell_path(id)];
        while let Some(at) = paths.pop() {
            if at.is_dir() {
                paths.extend(std::fs::read_dir(&at).unwrap().map(|entry| entry.unwrap().path()));
                continue;
            }
            let bytes = std::fs::read(&at).unwrap();
            let held = bytes.windows(64).any(|window| window == &pattern(seed)[..64]);
            assert!(!held, "{} holds what its VM wrote", at.display());
        }
    }

    let shred = |daemon: &mut Daemon| {
        let request = ShredTenantRequest { tenant: 1 };
        let shredded = daemon.runtime.block_on(daemon.client.shred_tenant(request)).unwrap();
        let ShredTenantResponse { shredded_now, forgotten } = shredded.into_inner();
        (shredded_now, forgotten.unwrap().stores)
    };
    assert_eq!(shred(&mut daemon), (true, 1));
    assert_eq!((read(&vms[0], 1), read(&vms[1], 2)), (Err(Errno::EACCES), Ok(true)));
    assert_eq!(shred(&mut daemon), (false, 0));

    // Once the daemon lets it go, which it does after its VM process's connection went (board 8282).
    drop(vms);
    assert_eq!(failure(daemon.stat_once_closed(ours)), (Code::PermissionDenied, Errno::EACCES));

    // Default tenant 0 is refused with INVALID_ARGUMENT / EINVAL.
    let zero =
        daemon.runtime.block_on(daemon.client.shred_tenant(ShredTenantRequest { tenant: 0 }));
    assert_eq!(failure(zero.map(drop)), (Code::InvalidArgument, Errno::EINVAL));
}

/// Standalone keyless `shred_tenant_bucket` (without calling `ShredTenant` or `ForgetTenantKeys`
/// on the daemon) tombstones the bucket KMS and publishes `TenantShredded` to the root bucket's
/// `events` object; the running daemon's background poller detects it and halts open cells of
/// that tenant (`EACCES`) while leaving other tenants' open cells healthy.
#[test]
fn standalone_shred_halts_open_cells_on_uncalled_daemon_via_events() {
    let master: String = (0..32).map(|byte| format!("{byte:02x}")).collect();
    // SAFETY: nextest runs each test in a process of its own before daemon threads start.
    unsafe {
        std::env::remove_var("CLADEFS_KMS");
        std::env::set_var("CLADEFS_MASTER_KEY", &master);
        std::env::set_var("CLADEFSD_EVENTS_POLL_MS", "50");
    }
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    cladefs::cladefsd::events::start().unwrap();
    let bucket_url = std::env::var("CLADEFSD_BUCKET").unwrap();
    let of = |tenant| CellId { tenant, id: 0 };
    let victim = daemon.create(of(11), None).unwrap();
    let survivor = daemon.create(of(12), None).unwrap();
    let vms = [daemon.open(victim), daemon.open(survivor)];
    for (seed, vm) in (1..).zip(&vms) {
        write(vm, &pattern(seed), b"events");
    }
    let read = |vm: &CellClient, seed: u64| {
        let mut data = vec![0; 64 << 10];
        let extent = cladefs::ExtentId { cell: vm.cell(), sequence: 1 };
        let done = vm.read(extent, 0, &mut [cladefs::VolatileSliceMut::from(&mut data[..])]);
        done.map(|()| data == pattern(seed))
    };
    assert_eq!((read(&vms[0], 1), read(&vms[1], 2)), (Ok(true), Ok(true)));

    // Shred tenant 11 directly at the bucket without calling the daemon and without needing
    // CLADEFS_MASTER_KEY (unset it before calling the standalone API).
    // SAFETY: single test process, daemon already read its env on boot.
    unsafe {
        std::env::remove_var("CLADEFS_MASTER_KEY");
    }
    let destroyed = cladefs::bucket::shred_tenant_bucket(&bucket_url, 11).unwrap();
    assert_eq!(destroyed, bucket_cell::bucket_kms::Destroyed::Now);

    // Wait up to 5 s for the daemon's events poller to detect TenantShredded(11) and halt victim.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if read(&vms[0], 1) == Err(Errno::EACCES) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "daemon events poller did not halt shredded tenant 11 within 5s"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(read(&vms[1], 2), Ok(true));
}

/// board 11665: when `CellShredded` events are trimmed from the bounded `events` tail before a
/// daemon polls (`update.skipped == true`), the daemon reconciles its open cells against their
/// bucket heads (`head.shredded`) and halts shredded cells with `EACCES` while healthy cells stay
/// open.
#[test]
fn trimmed_cell_shred_event_reconciles_open_cell_via_head_get() {
    let dir = tempfile::tempdir().unwrap();
    let bucket_dir = dir.path().join("bucket");
    std::fs::create_dir(&bucket_dir).unwrap();
    let bucket_url = format!("file://{}", bucket_dir.display());
    let master: String = (0..32).map(|byte| format!("{byte:02x}")).collect();
    // SAFETY: nextest runs each test in a process of its own before daemon threads start.
    unsafe {
        std::env::remove_var("CLADEFS_KMS");
        std::env::set_var("CLADEFS_MASTER_KEY", &master);
        std::env::set_var("CLADEFSD_BUCKET", &bucket_url);
        std::env::set_var("CLADEFSD_TOKEN", "1");
        std::env::set_var("CLADEFSD_EVENTS_POLL_MS", "50");
    }
    let mut daemon = Daemon::new(dir.path());
    cladefs::cladefsd::events::start().unwrap();
    let of = |tenant| CellId { tenant, id: 0 };
    let victim_cell = daemon.create(of(13), None).unwrap();
    let survivor_cell = daemon.create(of(13), None).unwrap();
    let vms = [daemon.open(victim_cell), daemon.open(survivor_cell)];
    for (seed, vm) in (1..).zip(&vms) {
        write(vm, &pattern(seed), b"events");
    }
    daemon.sync(victim_cell).unwrap();
    daemon.sync(survivor_cell).unwrap();
    let read = |vm: &cladefs::CellClient, seed: u64| {
        let mut data = vec![0; 64 << 10];
        let extent = cladefs::ExtentId { cell: vm.cell(), sequence: 1 };
        let done = vm.read(extent, 0, &mut [cladefs::VolatileSliceMut::from(&mut data[..])]);
        done.map(|()| data == pattern(seed))
    };
    assert_eq!((read(&vms[0], 1), read(&vms[1], 2)), (Ok(true), Ok(true)));

    // First publish an unrelated event so the daemon's poller advances `last_seq = 1`.
    let bucket = bucket_cell::object_bucket::ObjectBucket::from_url(&bucket_url).unwrap();
    daemon.runtime.block_on(async {
        bucket_cell::events::publish(
            &bucket,
            bucket_cell::events::Publish::CellShredded(cladefs::CellId { tenant: 99, id: 1 }),
        )
        .await
        .unwrap();
    });
    std::thread::sleep(std::time::Duration::from_millis(150));

    // Shred `victim_cell`'s head directly and advance `events` past `MAX_EVENTS` so `victim_cell`'s
    // `CellShredded` event is trimmed out of the tail (`oldest_seq = 2000 > last_seq + 1`).
    daemon.runtime.block_on(async {
        use bucket_cell::bucket::{Bucket as _, PutMode};
        let removed = bucket_cell::keys::shred(&bucket, victim_cell).await.unwrap();
        assert_eq!(removed, 1);
        let trimmed = bucket_cell::events::EventLog {
            latest_seq: 2000,
            oldest_seq: 2000,
            events: vec![bucket_cell::events::Event {
                seq: 2000,
                unix_ms: 1,
                kind: Some(bucket_cell::events::EventKind::CellShredded(
                    bucket_cell::events::CellShredded { tenant: 99, cell: 2 },
                )),
            }],
            ..Default::default()
        };
        let bytes = bucket_cell::events::encode(&trimmed);
        bucket.put(bucket_cell::layout::EVENTS, bytes.into(), PutMode::Overwrite).await.unwrap();
    });

    // The poller sees `update.skipped == true`, reconciles open cells via `head::get`, and halts
    // `victim` while leaving `survivor` unaffected.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if read(&vms[0], 1) == Err(Errno::EACCES) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "daemon events poller did not reconcile trimmed CellShredded within 5s"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(read(&vms[1], 2), Ok(true));
}

/// board 8487: a tenant added to the KMS's key overrides file (`keys=FILE`) while the daemon runs
/// wraps under its own key (BYOK) from the next call on, `ReloadKms` reading the file again, no
/// restart: before it, tenant 7's cells are refused, as the template names a key OpenBao lacks.
/// A file with a line that is no override is refused (INVALID_ARGUMENT, EINVAL), naming the line,
/// the overrides in use kept; so is renaming the key of a tenant with cells, or dropping its
/// override (FAILED_PRECONDITION, EBUSY), as its cells would open no more (board 2062).
#[test]
fn key_overrides_reload_without_a_restart_openbao() {
    use extent_keys::testing::openbao::DevServer;
    let Some(bao) = DevServer::start() else { return };
    bao.transit("transit");
    for key in ["cladefs-tenant-1", "byok-7"] {
        bao.bao(&["write", "-f", &format!("transit/keys/{key}")]);
    }
    bao.policy("cladefs", &DevServer::cladefs_policy("transit", "*"));
    let dir = tempfile::tempdir().unwrap();
    let keys = dir.path().join("keys");
    std::fs::write(&keys, "# tenants with keys of their own\n").unwrap();
    let url = bao.url("transit", &format!("key=cladefs-tenant-{{tenant}}&keys={}", keys.display()));
    let bucket = dir.path().join("bucket");
    std::fs::create_dir(&bucket).unwrap();
    // SAFETY: as in tenants_encrypted_under_the_bucket_kms_and_shredded_through_the_control_api.
    unsafe {
        std::env::remove_var("CLADEFS_MASTER_KEY");
        std::env::set_var("CLADEFS_KMS", &url);
        std::env::set_var(extent_keys::kms::openbao::TOKEN_ENV, bao.token("cladefs"));
        std::env::set_var("CLADEFSD_BUCKET", format!("file://{}", bucket.display()));
    }
    let mut daemon = Daemon::new(dir.path());
    let of = |tenant| CellId { tenant, id: 0 };
    let reload = |daemon: &mut Daemon| {
        let reloaded = daemon.runtime.block_on(daemon.client.reload_kms(ReloadKmsRequest {}));
        reloaded.map(tonic::Response::into_inner)
    };
    let seven = base32_id::Short(7).to_string();
    daemon.create(of(1), None).unwrap();
    let early = daemon.create(of(7), None);
    assert_eq!(failure(early).0, Code::PermissionDenied);

    std::fs::write(&keys, format!("# tenants with keys of their own\n{seven} byok-7\n")).unwrap();
    let reloaded = reload(&mut daemon).unwrap();
    assert_eq!(reloaded, ReloadKmsResponse { renamed: vec![7], master_keys: vec![] });
    let byok = daemon.create(of(7), None).unwrap();
    let vm = daemon.open(byok);
    write(&vm, &pattern(7), b"byok");
    drop(vm);
    daemon.sync(byok).unwrap();
    assert_eq!(reload(&mut daemon).unwrap(), ReloadKmsResponse::default());

    let refusal = |daemon: &mut Daemon| {
        let refused = reload(daemon).unwrap_err();
        let errno = cladefs::cladefsd::control::errno(&refused).unwrap();
        (refused.code(), errno, refused.message().to_owned())
    };
    std::fs::write(&keys, format!("{seven} byok-7\nnot-base-32! byok-8\n")).unwrap();
    let malformed =
        format!("reloading the KMS: {url}: the key overrides' line 2: no tenant in base 32");
    assert_eq!(refusal(&mut daemon), (Code::InvalidArgument, Errno::EINVAL, malformed));
    for overrides in [format!("{seven} byok-7b\n"), String::new()] {
        std::fs::write(&keys, overrides).unwrap();
        let (code, errno, message) = refusal(&mut daemon);
        assert_eq!((code, errno), (Code::FailedPrecondition, Errno::EBUSY));
        let renamed = format!(
            "reloading the KMS: {url}: tenant 7's KEK would be renamed, and it has cells in the \
             host's bucket ({seven}/"
        );
        assert!(message.starts_with(&renamed), "{message}");
    }
    // Every refusal kept the overrides in use: tenant 7's next cell is under byok-7 still, which
    // OpenBao has, byok-7b not.
    daemon.create(of(7), None).unwrap();
}

/// board 8487: master keys added to `CLADEFS_MASTER_KEY_FILE` while the daemon runs are its from
/// the next KEK object on, `ReloadKms` reading the file again; removing one is refused while a
/// tenant's KEK object is under it, or the registry of buckets records it as the bucket's (board
/// 8483), FAILED_PRECONDITION (EBUSY), and done once a rotation moved both to another; a file
/// holding no key is refused (INVALID_ARGUMENT, EINVAL), the keys in use kept.
#[test]
fn master_keys_reload_without_a_restart_keeping_those_in_use() {
    use bucket_cell::identity::{self, Details};
    use extent_keys::kms::master::MasterKeys;
    let hex = |byte: u8| format!("{byte:02x}").repeat(32);
    let (a, b, c) = (hex(0xa1), hex(0xb2), hex(0xc3));
    let id = |key: &str| MasterKeys::parse(key, "a test's").unwrap().newest().id().to_string();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("master-keys");
    std::fs::write(&file, &a).unwrap();
    std::fs::create_dir(dir.path().join("bucket")).unwrap();
    let url = format!("file://{}", dir.path().join("bucket").display());
    let bucket = bucket_cell::object_bucket::ObjectBucket::from_url(&url).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let details = Details { master_key_id: Some(id(&a)), ..Details::default() };
    runtime.block_on(identity::format(&bucket, &details)).unwrap();
    // SAFETY: as in tenants_encrypted_under_the_bucket_kms_and_shredded_through_the_control_api.
    unsafe {
        std::env::remove_var("CLADEFS_KMS");
        std::env::remove_var("CLADEFS_MASTER_KEY");
        std::env::set_var("CLADEFS_MASTER_KEY_FILE", &file);
        std::env::set_var("CLADEFSD_BUCKET", &url);
    }
    let mut daemon = Daemon::new(dir.path());
    let of = |tenant| CellId { tenant, id: 0 };
    let reload = |daemon: &mut Daemon| {
        let reloaded = daemon.runtime.block_on(daemon.client.reload_kms(ReloadKmsRequest {}));
        reloaded.map(tonic::Response::into_inner)
    };
    let refusal = |daemon: &mut Daemon| {
        let refused = reload(daemon).unwrap_err();
        let errno = cladefs::cladefsd::control::errno(&refused).unwrap();
        (refused.code(), errno, refused.message().to_owned())
    };
    let changed = |added: &[&str], removed: &[&str]| ReloadKmsResponse {
        renamed: vec![],
        master_keys: vec![MasterKeysReloaded {
            bucket: 1,
            added: added.iter().map(|key| id(key)).collect(),
            removed: removed.iter().map(|key| id(key)).collect(),
        }],
    };
    daemon.create(of(1), None).unwrap();

    std::fs::write(&file, format!("{b}\n{a}\n")).unwrap();
    assert_eq!(reload(&mut daemon).unwrap(), changed(&[&b], &[]));
    daemon.create(of(2), None).unwrap();

    let busy = |why: String| (Code::FailedPrecondition, Errno::EBUSY, why);
    std::fs::write(&file, &a).unwrap();
    let sealed = format!(
        "reloading the KMS: bucket 1: tenant 2's KEK object is under master key {}: rotate them \
         under another first (cladefs-gc --rotate-master-key), then remove it",
        id(&b)
    );
    assert_eq!(refusal(&mut daemon), busy(sealed));
    std::fs::write(&file, &b).unwrap();
    let recorded = format!(
        "reloading the KMS: bucket 1: the registry of buckets records master key {} as its \
         tenants' KEKs': rotate them under another first (cladefs-gc --rotate-master-key), then \
         remove it",
        id(&a)
    );
    assert_eq!(refusal(&mut daemon), busy(recorded));

    let both = MasterKeys::parse(&format!("{b},{a}"), "a test's").unwrap();
    let rotated = runtime.block_on(bucket_cell::bucket_kms::rotate(&bucket, &both)).unwrap();
    assert_eq!((rotated.rewrapped, rotated.newest, rotated.refused.len()), (1, 1, 0));
    let recorded = runtime.block_on(identity::record_master_key(&bucket, 1, &id(&b)));
    assert!(recorded.unwrap().is_some());
    assert_eq!(reload(&mut daemon).unwrap(), changed(&[], &[&a]));
    daemon.create(of(3), None).unwrap();

    std::fs::write(&file, &c).unwrap();
    let recorded = format!(
        "reloading the KMS: bucket 1: the registry of buckets records master key {} as its \
         tenants' KEKs': rotate them under another first (cladefs-gc --rotate-master-key), then \
         remove it",
        id(&b)
    );
    assert_eq!(refusal(&mut daemon), busy(recorded));
    std::fs::write(&file, "not a key").unwrap();
    let malformed = format!(
        "reloading the KMS: bucket 1's bucket KMS: CLADEFS_MASTER_KEY_FILE {}: key 1 of 3: not 64 \
         hexadecimal digits",
        file.display()
    );
    assert_eq!(refusal(&mut daemon), (Code::InvalidArgument, Errno::EINVAL, malformed));
    daemon.create(of(4), None).unwrap();
}

/// What a guest of today says of its stats, as its Fxfs and its driver write them: its device's
/// and its window's requests timed (a count, nanoseconds, a histogram of 24 buckets each), and the
/// rest of its counters, some 50 lines and 12 histograms in all.
#[cfg(target_os = "linux")]
fn todays_guest() -> String {
    let mut text = String::new();
    for (n, timed) in ["device_reads", "device_writes", "device_commits", "seal_waits", "seals"]
        .into_iter()
        .chain(["writes_held_by_seals", "window_map_requests", "window_faults", "window_unmaps"])
        .chain(["window_shrinks", "window_prefaults", "window_private_requests"])
        .enumerate()
    {
        let count = 1_000 * (n as u64 + 1);
        text += &format!("{timed} {count}\n{timed}_ns {}\n{timed}_hist ", count * 45_000);
        let buckets: Vec<String> =
            (0..24).map(|bucket| ((count >> bucket) % 900).to_string()).collect();
        text += &(buckets.join(",") + "\n");
    }
    for n in 0..26 {
        text += &format!("vfs_counter_{n} {}\n", 12_345 * n);
    }
    text + "commit 0123456789abcdef0123456789abcdef01234567\n"
}

/// What a guest says of its stats near the most it may (`MAX_STATS`, 64 KiB): 2,000 counters, and
/// 100 histograms of 24 buckets.
#[cfg(target_os = "linux")]
fn full_guest() -> String {
    let mut text = String::new();
    for n in 0..2_000 {
        text += &format!("c{n:04} {}\n", 1_234_567_890_123u64 + n);
    }
    for n in 0..100 {
        let buckets: Vec<String> = (0..24).map(|bucket| (1_234_567 + bucket).to_string()).collect();
        text += &format!("h{n:03}_hist {}\n", buckets.join(","));
    }
    assert!(text.len() < 64 << 10, "{}", text.len());
    text
}

/// board 8581: GetStats of VM processes past gRPC's default limit of a message, 4 MiB, which the
/// clients failed with OUT_OF_RANGE as a host was busiest, is taken through the tonic client, each
/// VM process's size measured: one of today's guest's some 2 KiB (1,960 bytes), one near the most
/// a guest says some 40 KiB (40,568), as the .proto says. Past the most the daemon sends, here lowered to 2 MiB, it is
/// refused, RESOURCE_EXHAUSTED (EMSGSIZE), saying what to leave out, until enough is.
#[test]
#[cfg(target_os = "linux")]
fn stats_of_many_vms_are_taken_and_bounded() {
    use std::sync::Arc;
    const VMS: usize = 120;
    // Each VM process's cell and connection hold files open, past the usual 1,024 (board 8595).
    cladefs::cladefsd::raise_open_files_limit().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let (today, full) = (todays_guest(), full_guest());
    let vms: Vec<Arc<CellClient>> = (0..=VMS)
        .map(|n| {
            let (_, vm) = daemon.open_new(0, CAPACITY);
            let vm = Arc::new(vm);
            let guest = if n == 0 { today.clone() } else { full.clone() };
            // Its guest answers each ask, off the thread the ask comes on.
            let answering = Arc::downgrade(&vm);
            vm.on_stats_asked(move |ask| {
                let (vm, guest) = (answering.clone(), guest.clone());
                std::thread::spawn(move || {
                    if let Some(vm) = vm.upgrade() {
                        vm.answer_stats(ask, Some(&guest)).unwrap();
                    }
                });
                true
            });
            vm
        })
        .collect();
    let client = cladefs::testing::Control::connect(dir.path().join("control.sock")).unwrap();
    let asking = GetStatsRequest { wait_ms: 30_000, ..GetStatsRequest::default() };

    let stats = client.call(async |client| client.get_stats(asking.clone()).await).unwrap();
    assert_eq!(stats.vms.len(), VMS + 1);
    assert!(stats.vms.iter().all(|vm| vm.answered), "a VM process not answering");
    let bytes = prost::Message::encoded_len(&stats);
    assert!(bytes > 4 << 20 && bytes < cladefs::cladefsd::control::MAX_MESSAGE_BYTES, "{bytes}");
    let (todays, fullest) =
        (prost::Message::encoded_len(&stats.vms[0]), prost::Message::encoded_len(&stats.vms[1]));
    eprintln!(
        "GetStats: {bytes} bytes; a VM process of today's guest {todays}, of a full one {fullest}"
    );
    assert!((1 << 10..3 << 10).contains(&todays), "today's guest's VM process: {todays} bytes");
    assert!((32 << 10..48 << 10).contains(&fullest), "a full guest's VM process: {fullest} bytes");
    let listed = client.call(async |client| client.list_cells(ListCellsRequest {}).await).unwrap();
    assert_eq!(listed.stores.len(), VMS + 1);

    fail::cfg("cladefsd::most_answer", "return(2097152)").unwrap();
    let mut asked = |leave_out: [bool; 3]| {
        let [leave_out_histograms, leave_out_guests, leave_out_vms] = leave_out;
        let request = GetStatsRequest {
            leave_out_histograms,
            leave_out_guests,
            leave_out_vms,
            ..asking.clone()
        };
        let answered = daemon.runtime.block_on(daemon.client.get_stats(request));
        answered.map(tonic::Response::into_inner)
    };
    for (leave_out, said) in [
        ([false; 3], "leave out the guests' histograms (leave_out_histograms)"),
        ([true, false, false], "leave out the guests' stats (leave_out_guests)"),
    ] {
        let refused = asked(leave_out).unwrap_err();
        assert_eq!(failure::<()>(Err(refused.clone())), (Code::ResourceExhausted, Errno::EMSGSIZE));
        assert!(refused.message().starts_with("GetStats's answer would be "), "{refused:?}");
        assert!(
            refused
                .message()
                .ends_with(&format!(" past the most the control API sends, 2097152: {said}")),
            "{refused:?}"
        );
    }
    let without = asked([false, true, false]).unwrap();
    assert_eq!(without.vms.len(), VMS + 1);
    assert!(without.vms.iter().all(|vm| vm.guest.is_none() && vm.answered));
    let without = asked([false, false, true]).unwrap();
    assert_eq!((without.vms.len(), without.build.as_str()), (0, cladefs::BUILD_COMMIT));
    fail::remove("cladefsd::most_answer");
    drop(vms);
}

/// board 11222: memory cells imported, versioned, synced and forked through the control API alone,
/// without touching `MemoryCell::import` in Rust: zero pages stored as holes, contents read back
/// intact through a fork, and bad files / missing files refused.
#[test]
#[cfg(target_os = "linux")]
fn memory_cells_through_the_control_api() {
    // SAFETY: nextest runs each test in a process of its own, whose other threads don't read the
    // environment yet.
    unsafe {
        std::env::set_var("CLADEFSD_BUCKET", "memory:///cells");
        std::env::set_var("CLADEFSD_TOKEN", "1");
    }
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let ps = cladefs::pages::page_size() as usize;
    let pages = 64usize;
    let size = (pages * ps) as u64;

    // Sparse memory image: mostly zero pages, with 4 non-zero pages scattered across it.
    let mut expected = vec![0u8; size as usize];
    for &(page, seed) in &[(0usize, 0x11u8), (3, 0x22), (31, 0x33), (63, 0x44)] {
        for (i, byte) in expected[page * ps..(page + 1) * ps].iter_mut().enumerate() {
            *byte = seed ^ (i % 251) as u8;
        }
    }
    let mem_file = dir.path().join("snapshot.mem");
    std::fs::write(&mem_file, &expected).unwrap();

    // Import with `make_version: true` in one call.
    let imported = daemon
        .runtime
        .block_on(daemon.client.import_memory(ImportMemoryRequest {
            file: name(&mem_file),
            tenant: 0,
            make_version: true,
            ephemeral: false,
        }))
        .unwrap()
        .into_inner();
    assert_eq!(imported.tenant, 0);
    assert_ne!(imported.cell, 0);
    assert_ne!(imported.version, 0);
    let mem_cell = CellId { tenant: imported.tenant, id: imported.cell };

    // Zero pages are discarded as holes: used bytes are far smaller than the memory file size.
    let stat = daemon.stat(mem_cell).unwrap();
    assert_eq!(stat.cell, imported.cell);
    assert!(stat.used < size / 2, "expected zero pages discarded as holes, got used={}", stat.used);

    // Sync to the bucket and fork the version through the control API, then read back every byte.
    daemon.sync(mem_cell).unwrap();
    let forked = daemon
        .runtime
        .block_on(daemon.client.fork_cell(ForkCellRequest {
            tenant: imported.tenant,
            cell: imported.cell,
            version: imported.version,
            ..Default::default()
        }))
        .unwrap()
        .into_inner();
    assert_ne!(forked.cell, imported.cell);
    let forked_cell = CellId { tenant: forked.tenant, id: forked.cell };
    let opened =
        cladefs::memory_cell::MemoryCell::open_cell(&daemon.vms, forked_cell, size).unwrap();
    let mut got = vec![0u8; size as usize];
    opened.read(0, &mut got).unwrap();
    assert!(got == expected, "forked memory cell contents differ from imported file");
    opened.close().unwrap();

    // Import without `make_version`, then call `MakeVersion` via the control API.
    let imported2 = daemon
        .runtime
        .block_on(daemon.client.import_memory(ImportMemoryRequest {
            file: name(&mem_file),
            tenant: 0,
            make_version: false,
            ephemeral: false,
        }))
        .unwrap()
        .into_inner();
    assert_eq!(imported2.version, 0);
    let ver = daemon
        .runtime
        .block_on(daemon.client.make_version(MakeVersionRequest {
            tenant: imported2.tenant,
            cell: imported2.cell,
            keep_last: 0,
        }))
        .unwrap()
        .into_inner()
        .version;
    assert_ne!(ver, 0);

    // Error cases: missing file (ENOENT), unaligned/empty file (EINVAL).
    let missing = daemon.runtime.block_on(daemon.client.import_memory(ImportMemoryRequest {
        file: name(&dir.path().join("no-such-file.mem")),
        tenant: 0,
        make_version: false,
        ephemeral: false,
    }));
    assert_eq!(failure(missing), (Code::NotFound, Errno::ENOENT));

    let bad_file = dir.path().join("unaligned.mem");
    std::fs::write(&bad_file, vec![1u8; 123]).unwrap();
    let bad = daemon.runtime.block_on(daemon.client.import_memory(ImportMemoryRequest {
        file: name(&bad_file),
        tenant: 0,
        make_version: false,
        ephemeral: false,
    }));
    assert_eq!(failure(bad), (Code::InvalidArgument, Errno::EINVAL));

    // Importing directly for tenant 7 assigns tenant 7 on a daemon-chosen disk.
    daemon.grant(7, None).unwrap();
    let right_tenant = daemon
        .runtime
        .block_on(daemon.client.import_memory(ImportMemoryRequest {
            file: name(&mem_file),
            tenant: 7,
            make_version: true,
            ephemeral: false,
        }))
        .unwrap()
        .into_inner();
    assert_eq!(right_tenant.tenant, 7);
    assert_ne!(right_tenant.version, 0);

    // Ephemeral memory import: rejects make_version=true, otherwise stays local with ephemeral=true.
    let bad_ephemeral = daemon.runtime.block_on(daemon.client.import_memory(ImportMemoryRequest {
        file: name(&mem_file),
        tenant: 0,
        make_version: true,
        ephemeral: true,
    }));
    assert_eq!(failure(bad_ephemeral), (Code::InvalidArgument, Errno::EINVAL));
    let imported_eph = daemon
        .runtime
        .block_on(daemon.client.import_memory(ImportMemoryRequest {
            file: name(&mem_file),
            tenant: 0,
            make_version: false,
            ephemeral: true,
        }))
        .unwrap()
        .into_inner();
    assert_eq!(imported_eph.version, 0);
    let eph_id = CellId { tenant: imported_eph.tenant, id: imported_eph.cell };
    let eph_stat = daemon.stat(eph_id).unwrap();
    assert!(eph_stat.ephemeral);
    assert!(!eph_stat.uploading);
    // No placement holds an ephemeral cell (board 13879).
    assert_eq!(eph_stat.placement_token, 0);
    let eph_open = cladefs::memory_cell::MemoryCell::open_ephemeral_existing(
        &daemon.vms,
        std::path::Path::new(&eph_id.to_string()),
        Some(eph_id),
        size,
    )
    .unwrap();
    let mut got_eph = vec![0u8; size as usize];
    eph_open.read(0, &mut got_eph).unwrap();
    assert!(got_eph == expected);
    eph_open.close().unwrap();
    daemon.delete(eph_id).unwrap();

    // Standalone ephemeral CreateCell via control API.
    let created_eph = daemon
        .runtime
        .block_on(daemon.client.create_cell(CreateCellRequest {
            capacity: CAPACITY,
            kind: CellKind::Memory.into(),
            ephemeral: true,
            ..Default::default()
        }))
        .unwrap()
        .into_inner();
    assert_ne!(created_eph.cell, 0);
    let created_eph_id = CellId { tenant: created_eph.tenant, id: created_eph.cell };
    let stat_eph = daemon.stat(created_eph_id).unwrap();
    assert!(stat_eph.ephemeral);
    assert!(!stat_eph.uploading);
    daemon.delete(created_eph_id).unwrap();
    let bad_create_eph = daemon.runtime.block_on(daemon.client.create_cell(CreateCellRequest {
        capacity: CAPACITY,
        token: Some(1),
        ephemeral: true,
        ..Default::default()
    }));
    assert_eq!(failure(bad_create_eph), (Code::InvalidArgument, Errno::EINVAL));
}

/// doc-29: Fast WAL cells are refused with ENOTSUP without CLADEFSD_FAST_WAL_BUCKET and with
/// EINVAL on invalid durability mode combinations.
#[test]
fn fast_wal_refused_without_bucket_and_on_invalid_combinations() {
    // SAFETY: nextest runs each test in a process of its own.
    unsafe {
        std::env::set_var("CLADEFSD_BUCKET", "memory:///fast-wal-ctl-refused");
        std::env::set_var("CLADEFSD_TOKEN", "1");
        std::env::remove_var("CLADEFSD_FAST_WAL_BUCKET");
    }
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let create = |daemon: &mut Daemon, request: CreateCellRequest| {
        daemon.runtime.block_on(daemon.client.create_cell(request)).map(|made| made.into_inner())
    };

    // Without CLADEFSD_FAST_WAL_BUCKET, fast_wal = true is refused with ENOTSUP before making anything.
    let refused = create(
        &mut daemon,
        CreateCellRequest {
            capacity: CAPACITY,
            fast_wal: Some(true),
            fsync: FsyncLevel::FastWal.into(),
            ..CreateCellRequest::default()
        },
    );
    assert_eq!(failure(refused), (Code::FailedPrecondition, Errno::ENOTSUP));
    assert!(daemon.list().is_empty());

    // Invalid combinations are refused with EINVAL and create no cell.
    let invalid = [(None, FsyncLevel::FastWal), (Some(false), FsyncLevel::FastWal)];
    for (fast_wal, fsync) in invalid {
        let res = create(
            &mut daemon,
            CreateCellRequest {
                capacity: CAPACITY,
                fast_wal,
                fsync: fsync.into(),
                ..CreateCellRequest::default()
            },
        );
        assert_eq!(failure(res), (Code::InvalidArgument, Errno::EINVAL), "{fast_wal:?} {fsync:?}");
        assert!(daemon.list().is_empty());
    }
}

/// doc-29: Fast WAL cells created, written, stated, and taken through the control API.
#[test]
fn cells_are_made_fast_wal_through_the_control_api() {
    let dir = tempfile::tempdir().unwrap();
    let fast_wal_dir = dir.path().join("fast-wal-store");
    std::fs::create_dir_all(&fast_wal_dir).unwrap();
    let fast_wal_url = format!("file://{}", fast_wal_dir.display());
    // SAFETY: nextest runs each test in a process of its own.
    unsafe {
        std::env::set_var("CLADEFSD_BUCKET", "memory:///fast-wal-ctl");
        std::env::set_var("CLADEFSD_TOKEN", "1");
        std::env::set_var("CLADEFSD_FAST_WAL_BUCKET", &fast_wal_url);
    }
    let dir2 = tempfile::tempdir().unwrap();
    let mut daemon2 = Daemon::new(dir2.path());
    let create = |daemon: &mut Daemon, request: CreateCellRequest| {
        daemon.runtime.block_on(daemon.client.create_cell(request)).map(|made| made.into_inner())
    };
    let made = create(
        &mut daemon2,
        CreateCellRequest {
            capacity: CAPACITY,
            fast_wal: Some(true),
            fsync: FsyncLevel::FastWal.into(),
            ..CreateCellRequest::default()
        },
    )
    .unwrap();
    assert_ne!(made.cell, 0);
    let first = CellId { tenant: made.tenant, id: made.cell };

    let vm = daemon2.open(first);
    write(&vm, &pattern(1), b"fast-wal-data");
    let stat = daemon2.stat(first).unwrap();
    assert!(stat.fast_wal);
    assert_eq!(stat.fast_wal_bucket, fast_wal_url);
    assert!(!stat.fast_wal_degraded);
    assert_eq!(stat.fsync(), FsyncLevel::FastWal);
    drop(vm);
    let closed = daemon2.wait_closed(first);
    assert!(closed.fast_wal);
    assert_eq!(closed.fsync(), FsyncLevel::FastWal);

    // Drop the local copy and re-take the same cell at a higher token (inherits Fast WAL from head).
    daemon2.drop_cell(first).unwrap();
    create(
        &mut daemon2,
        CreateCellRequest {
            capacity: CAPACITY,
            tenant: made.tenant,
            cell: made.cell,
            token: Some(2),
            ..CreateCellRequest::default()
        },
    )
    .unwrap();
    let vm2 = daemon2.open(first);
    let stat2 = daemon2.stat(first).unwrap();
    assert!(stat2.fast_wal);
    assert_eq!(stat2.fsync(), FsyncLevel::FastWal);
    assert_eq!(stat2.fast_wal_bucket, fast_wal_url);
    drop(vm2);

    // Explicit fast_wal=true and FSYNC_LEVEL_FAST_WAL keeps FastWal fsync.
    let made3 = create(
        &mut daemon2,
        CreateCellRequest {
            capacity: CAPACITY,
            fast_wal: Some(true),
            fsync: FsyncLevel::FastWal.into(),
            ..CreateCellRequest::default()
        },
    )
    .unwrap();
    let zero_rep = CellId { tenant: made3.tenant, id: made3.cell };
    let vm3 = daemon2.open(zero_rep);
    // Exercise guest fsync (`commit_ordered` -> `Waiting::FastWal` via `head.fast_wal.fsync`).
    let extent = cladefs::ExtentId { cell: vm3.cell(), sequence: 1 };
    let data = pattern(2);
    vm3.write(extent, 0, &[cladefs::VolatileSlice::from(&data[..])]).unwrap();
    vm3.commit_ordered(Some(b"zero-rep-fw")).unwrap();
    let cell3 = bucket_cell::layout::CellId { tenant: made3.tenant, id: made3.cell };
    // Epoch 1 (from `create_cell`) was empty and fenced by epoch 2 with the 36-byte `CFTF`
    // tombstone; epoch 2's segment 0 must already contain the committed record before returning.
    let wal_ep1 = fast_wal_dir.join(bucket_cell::layout::wal(cell3, 1, 0));
    assert_eq!(
        std::fs::metadata(&wal_ep1).unwrap().len(),
        local_cell::fast_wal::TOMBSTONE_LEN as u64
    );
    let wal_ep2 = fast_wal_dir.join(bucket_cell::layout::wal(cell3, 2, 0));
    let wal_len = std::fs::metadata(&wal_ep2)
        .expect("FSYNC_LEVEL_FAST_WAL must append to fast WAL before returning")
        .len();
    assert!(
        wal_len > local_cell::fast_wal::HEADER_LEN as u64,
        "fast WAL segment {wal_ep2:?} must contain the committed record (len={wal_len})"
    );
    let stat3 = daemon2.stat(zero_rep).unwrap();
    assert!(stat3.fast_wal);
    assert_eq!(stat3.fsync(), FsyncLevel::FastWal);
    drop(vm3);
    daemon2.wait_closed(zero_rep);

    // Hotbed review 118 #426: corrupt local WAL files of an active Fast WAL cell and verify
    // reopening recovers cleanly from the remote cold + fast WAL buckets.
    let zero_rep_dir = daemon2.cell_path(zero_rep);
    std::fs::write(zero_rep_dir.join("wal"), b"corrupt-local-wal-start-file").unwrap();
    let vm3_reopened = daemon2.open(zero_rep);
    assert_eq!(vm3_reopened.root().unwrap(), b"zero-rep-fw");
    drop(vm3_reopened);
}

/// hotbed question 547: multi-disk JBOD governance via `ListDisks` and `SetDiskGovernance`:
/// draining a disk routes new cells strictly to the remaining `READ_WRITE` disk while keeping
/// existing cells writable; marking all disks non-writable refuses new placements (`ENOSPC`);
/// per-disk caps/cache caps/dirty budgets update live; raw host paths on VM `OPEN` are refused
/// (`EINVAL`).
#[test]
fn disk_pool_governs_multiple_disks_placement_and_caps() {
    let dir = tempfile::tempdir().unwrap();
    let disk0 = dir.path().join("disk0");
    let disk1 = dir.path().join("disk1");
    let mut daemon = Daemon::with_disks(dir.path(), &[disk0.clone(), disk1.clone()]);

    let disks = daemon.list_disks().unwrap();
    assert_eq!(disks.len(), 2);
    assert_eq!(disks[0].id, "disk-0");
    assert_eq!(disks[1].id, "disk-1");
    assert_eq!(disks[0].placement(), DiskPlacementMode::DiskPlacementReadWrite);
    assert_eq!(disks[0].state(), DiskHealthMode::DiskHealthOnline);

    // Raw host paths on VM socket OPEN are refused with EINVAL when DiskPool is active.
    let raw = CellClient::connect(&daemon.vms, dir.path().join("sneaky-host-path"), CAPACITY);
    assert_eq!(raw.err().and_then(|e| e.errno()), Some(Errno::EINVAL));

    // Drain disk-0: all new cells must land on disk-1.
    daemon
        .set_disk_governance(SetDiskGovernanceRequest {
            id: "disk-0".into(),
            placement: Some(DiskPlacementMode::DiskPlacementDrain.into()),
            cap_bytes: Some(512 << 20),
            cache_cap_bytes: Some(128 << 20),
            dirty_budget_bytes: Some(64 << 20),
            ..Default::default()
        })
        .unwrap();
    let updated = daemon.list_disks().unwrap().into_iter().find(|d| d.id == "disk-0").unwrap();
    assert_eq!(updated.placement(), DiskPlacementMode::DiskPlacementDrain);
    assert_eq!(updated.cap_bytes, 512 << 20);
    assert_eq!(updated.cache_cap_bytes, 128 << 20);
    assert_eq!(updated.dirty_budget_bytes, 64 << 20);

    let c1 = daemon.create(NEW, None).unwrap();
    let p1 = daemon.cell_path(c1);
    assert!(p1.starts_with(disk1.canonicalize().unwrap()), "expected disk1, got {}", p1.display());

    // Mark disk-1 READ_ONLY as well: no disk accepts new cells -> ENOSPC.
    daemon
        .set_disk_governance(SetDiskGovernanceRequest {
            id: "disk-1".into(),
            placement: Some(DiskPlacementMode::DiskPlacementReadOnly.into()),
            ..Default::default()
        })
        .unwrap();
    let full = daemon.create(NEW, None);
    assert_eq!(failure(full), (Code::ResourceExhausted, Errno::ENOSPC));

    // Unknown disk ID -> ENOENT.
    let missing = daemon.set_disk_governance(SetDiskGovernanceRequest {
        id: "nvme99".into(),
        placement: Some(DiskPlacementMode::DiskPlacementDrain.into()),
        ..Default::default()
    });
    assert_eq!(failure(missing), (Code::NotFound, Errno::ENOENT));

    // Restore disk-0 to READ_WRITE and clear cap_bytes -> new cells land on disk-0.
    daemon
        .set_disk_governance(SetDiskGovernanceRequest {
            id: "disk-0".into(),
            placement: Some(DiskPlacementMode::DiskPlacementReadWrite.into()),
            clear_cap: true,
            ..Default::default()
        })
        .unwrap();
    let restored = daemon.list_disks().unwrap().into_iter().find(|d| d.id == "disk-0").unwrap();
    assert_eq!(restored.placement(), DiskPlacementMode::DiskPlacementReadWrite);
    assert_eq!(restored.cap_bytes, 0);
    let c2 = daemon.create(NEW, None).unwrap();
    let p2 = daemon.cell_path(c2);
    assert!(p2.starts_with(disk0.canonicalize().unwrap()), "expected disk0, got {}", p2.display());
}

/// hotbed question 547: `cladefsd --config cladefs.toml` boots from a TOML config file with
/// multiple `[[disks]]`, enforces per-disk caps and placement modes on startup, and exposes them
/// through `ListDisks`.
#[test]
#[cfg(target_os = "linux")]
fn cladefsd_boots_from_toml_config_with_per_disk_governance() {
    use std::process::Command;
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    let vm_sock = dir.path().join("cladefsd.sock");
    let ctl_sock = dir.path().join("control.sock");
    let nvme0 = dir.path().join("nvme0");
    let nvme1 = dir.path().join("nvme1");
    let config_path = dir.path().join("cladefs.toml");
    let toml = format!(
        r#"
[daemon]
vm_socket = "{vm}"
control_socket = "{ctl}"
token = 1

[bucket]
url = "memory:///toml-boot"

[disk]
headroom = "64 MiB"
part_cache = false

[[disks]]
id = "nvme0"
path = "{d0}"
cap = "1 GiB"
cache_cap = "256 MiB"
dirty_budget = "64 MiB"
placement = "read_write"

[[disks]]
id = "nvme1"
path = "{d1}"
cap = "2 GiB"
placement = "drain"
"#,
        vm = vm_sock.display(),
        ctl = ctl_sock.display(),
        d0 = nvme0.display(),
        d1 = nvme1.display(),
    );
    std::fs::write(&config_path, toml).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_cladefsd"))
        .args(["--config", config_path.to_str().unwrap()])
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while std::os::unix::net::UnixStream::connect(&ctl_sock).is_err() {
        assert!(child.try_wait().unwrap().is_none(), "cladefsd exited early");
        assert!(Instant::now() < deadline, "cladefsd never served");
        std::thread::sleep(Duration::from_millis(20));
    }

    let control = cladefs::testing::Control::connect(&ctl_sock).unwrap();
    let disks =
        control.call(async |client| client.list_disks(ListDisksRequest {}).await).unwrap().disks;
    assert_eq!(disks.len(), 2);
    assert_eq!(disks[0].id, "nvme0");
    assert_eq!(disks[0].cap_bytes, 1 << 30);
    assert_eq!(disks[0].cache_cap_bytes, 256 << 20);
    assert_eq!(disks[0].dirty_budget_bytes, 64 << 20);
    assert_eq!(disks[0].placement(), DiskPlacementMode::DiskPlacementReadWrite);
    assert_eq!(disks[1].id, "nvme1");
    assert_eq!(disks[1].cap_bytes, 2 << 30);
    assert_eq!(disks[1].placement(), DiskPlacementMode::DiskPlacementDrain);

    // Creating a cell must land on nvme0 (nvme1 is draining).
    let made = control
        .call(async |client| {
            client
                .create_cell(CreateCellRequest {
                    capacity: CAPACITY,
                    ..CreateCellRequest::default()
                })
                .await
        })
        .unwrap();
    assert_ne!(made.cell, 0);
    let expected_dir = nvme0
        .join("cells")
        .join(base32_id::Padded(made.tenant).to_string())
        .join(base32_id::Padded(made.cell).to_string());
    assert!(expected_dir.is_dir(), "expected cell under nvme0 at {}", expected_dir.display());

    // Cleanup daemon process.
    let _ = child.kill();
    let _ = child.wait();
}

/// hotbed review 135: concurrent `CreateCell(cell != 0)` calls for the same `(tenant, cell)` on a
/// multi-disk daemon serialize through `DiskPool::reserve_cell`: one succeeds, the retry returns
/// `EEXIST`, and only one copy of `cells/<TENANT>/<CELL>` exists across all disks.
#[test]
fn concurrent_named_create_cell_returns_eexist_and_leaves_one_copy() {
    let dir = tempfile::tempdir().unwrap();
    let disk0 = dir.path().join("disk0");
    let disk1 = dir.path().join("disk1");
    let mut daemon = Daemon::with_disks(dir.path(), &[disk0.clone(), disk1.clone()]);

    // Create and sync a cell, then drop it locally so it exists in the bucket ready to be taken.
    let cell = daemon.create(NEW, Some(1)).unwrap();
    daemon.sync(cell).unwrap();
    daemon.drop_cell(cell).unwrap();

    let mut c1 = daemon.client.clone();
    let mut c2 = daemon.client.clone();
    let req = CreateCellRequest {
        capacity: CAPACITY,
        token: Some(2),
        tenant: cell.tenant,
        cell: cell.id,
        ..CreateCellRequest::default()
    };
    let (r1, r2) =
        daemon.runtime.block_on(async { tokio::join!(c1.create_cell(req), c2.create_cell(req)) });
    let results = [r1, r2];
    let ok_count = results.iter().filter(|r| r.is_ok()).count();
    let eexist_count = results
        .iter()
        .filter(|r| r.as_ref().err().is_some_and(|s| s.code() == Code::AlreadyExists))
        .count();
    assert_eq!(ok_count, 1, "expected exactly 1 success: {results:?}");
    assert_eq!(eexist_count, 1, "expected exactly 1 AlreadyExists: {results:?}");

    let rel = std::path::Path::new("cells")
        .join(base32_id::Padded(cell.tenant).to_string())
        .join(base32_id::Padded(cell.id).to_string());
    let copies = [disk0.join(&rel), disk1.join(&rel)].into_iter().filter(|p| p.exists()).count();
    assert_eq!(copies, 1, "expected exactly one on-disk copy across disks");
}

/// `ForkCellRequest.persistent` (defaults to true; false = ephemeral) replaces the deprecated
/// `ephemeral` field while continuing to accept `ephemeral: true` and rejecting contradictory
/// `(ephemeral: true, persistent: Some(true))` with `EINVAL`.
#[test]
fn fork_cell_persistent_and_deprecated_ephemeral() {
    let dir = tempfile::tempdir().unwrap();
    let bucket = dir.path().join("bucket");
    let disk = dir.path().join("disk0");
    let ephemeral = dir.path().join("ephemeral");
    let vms = dir.path().join("cladefsd.sock");
    let control = dir.path().join("control.sock");
    std::fs::create_dir(&bucket).unwrap();
    std::fs::create_dir(&disk).unwrap();
    struct KillOnDrop(std::process::Child);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _process = KillOnDrop(
        std::process::Command::new(env!("CARGO_BIN_EXE_cladefsd"))
            .arg(&vms)
            .env("CLADEFSD_BUCKET", format!("file://{}/", bucket.display()))
            .env("CLADEFSD_TOKEN", "1")
            .env("CLADEFSD_ROOTS", &disk)
            .env("CLADEFSD_PART_CACHE", dir.path().join("cache"))
            .env("CLADEFSD_GRPC", &control)
            .env("CLADEFSD_EPHEMERAL", &ephemeral)
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::os::unix::net::UnixStream::connect(&vms).is_err() {
        assert!(std::time::Instant::now() < deadline, "daemon never served");
        std::thread::sleep(Duration::from_millis(20));
    }
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let mut client = runtime.block_on(control::connect(&control)).unwrap();

    // Create a base cell, make a version, and sync it to the bucket.
    let created = runtime
        .block_on(client.create_cell(CreateCellRequest {
            capacity: 4 << 20,
            kind: CellKind::Fxfs.into(),
            ..Default::default()
        }))
        .unwrap()
        .into_inner();
    let version = runtime
        .block_on(client.make_version(MakeVersionRequest {
            tenant: created.tenant,
            cell: created.cell,
            keep_last: 0,
        }))
        .unwrap()
        .into_inner()
        .version;
    runtime
        .block_on(client.sync_cell(SyncCellRequest { tenant: created.tenant, cell: created.cell }))
        .unwrap();

    let is_ephemeral_cell = |client: &mut control::proto::cladefsd_client::CladefsdClient<
        tonic::transport::Channel,
    >,
                             tenant: u64,
                             cell: u64| {
        runtime
            .block_on(client.stat_cell(StatCellRequest { tenant, cell }))
            .unwrap()
            .into_inner()
            .ephemeral
    };

    // 1. Default (persistent: None, ephemeral: false) -> persistent bucket-backed cell.
    let dst_default = runtime
        .block_on(client.fork_cell(ForkCellRequest {
            tenant: created.tenant,
            cell: created.cell,
            version,
            ..Default::default()
        }))
        .unwrap()
        .into_inner();
    assert!(
        !is_ephemeral_cell(&mut client, dst_default.tenant, dst_default.cell),
        "default fork must be persistent"
    );

    // 2. Explicit persistent: Some(true) -> persistent bucket-backed cell.
    let dst_persistent = runtime
        .block_on(client.fork_cell(ForkCellRequest {
            tenant: created.tenant,
            cell: created.cell,
            version,
            persistent: Some(true),
            ..Default::default()
        }))
        .unwrap()
        .into_inner();
    assert!(
        !is_ephemeral_cell(&mut client, dst_persistent.tenant, dst_persistent.cell),
        "persistent: true must be persistent"
    );

    // 3. Explicit persistent: Some(false) -> ephemeral cell.
    let dst_non_persistent = runtime
        .block_on(client.fork_cell(ForkCellRequest {
            tenant: created.tenant,
            cell: created.cell,
            version,
            persistent: Some(false),
            ..Default::default()
        }))
        .unwrap()
        .into_inner();
    assert!(
        is_ephemeral_cell(&mut client, dst_non_persistent.tenant, dst_non_persistent.cell),
        "persistent: false must be ephemeral"
    );

    // 4. Deprecated ephemeral: true (with persistent: None) still creates an ephemeral cell.
    #[allow(deprecated)]
    let dst_deprecated = runtime
        .block_on(client.fork_cell(ForkCellRequest {
            tenant: created.tenant,
            cell: created.cell,
            version,
            ephemeral: true,
            ..Default::default()
        }))
        .unwrap()
        .into_inner();
    assert!(
        is_ephemeral_cell(&mut client, dst_deprecated.tenant, dst_deprecated.cell),
        "deprecated ephemeral: true must still work"
    );

    // 5. Contradictory (ephemeral: true, persistent: Some(true)) -> EINVAL.
    #[allow(deprecated)]
    let conflict = runtime.block_on(client.fork_cell(ForkCellRequest {
        tenant: created.tenant,
        cell: created.cell,
        version,
        ephemeral: true,
        persistent: Some(true),
        ..Default::default()
    }));
    assert_eq!(failure(conflict), (Code::InvalidArgument, Errno::EINVAL));

    // 6. persistent: Some(false) without a version (forking live cell directly) -> EINVAL.
    let no_ver = runtime.block_on(client.fork_cell(ForkCellRequest {
        tenant: created.tenant,
        cell: created.cell,
        persistent: Some(false),
        ..Default::default()
    }));
    assert_eq!(failure(no_ver), (Code::InvalidArgument, Errno::EINVAL));
}
