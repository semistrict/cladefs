// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-43.3: the extent daemon's control API, gRPC over a Unix socket, as the host's VM manager
//! calls it, from a client in Rust: every call, and the errors a store a VM has open gives.
//!
//! The daemon serves on threads of this process: its control socket, and a VM socket, whose
//! connections, VM processes here, have stores open.

#![cfg(target_os = "linux")]

mod common;

use cladefs::Errno;
use cladefs::extentd::control::proto::*;
use common::daemon::{CAPACITY, Daemon, NEW, failure, name, write};
use common::pattern;
use std::path::Path;
use tonic::Code;

/// A store made, written by a VM, grown, forked, made a public base, synced to object storage and
/// taken from there to a new store, placed, and deleted, through the API.
#[test]
fn stores_through_the_control_api() {
    // SAFETY: nextest runs each test in a process of its own, whose other threads don't read the
    // environment yet.
    unsafe {
        std::env::set_var("CLADEFS_EXTENTD_REMOTE", "memory:///branches");
        std::env::set_var("CLADEFS_EXTENTD_TOKEN", "1");
    }
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let (base, fork, taken) =
        (dir.path().join("base"), dir.path().join("fork"), dir.path().join("taken"));

    let branch = daemon.create(&base, NEW, None).unwrap();
    assert_ne!(branch.id, 0);
    let vm = daemon.open(&base, CAPACITY);
    assert_eq!(vm.branch(), branch.id);
    write(&vm, &pattern(1), b"base");
    drop(vm);
    let stat = daemon.stat(&base).unwrap();
    assert_eq!(
        stat,
        StatStoreResponse {
            branch: branch.id,
            tenant: branch.cell.tenant,
            cell: branch.cell.id,
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
            // Closed, the stat opens it, as the daemon opens any store, taking its branch's head
            // after its making's and its VM's (TASK-165.9): what it committed, uploaded.
            uploading: true,
            taken_token: 1,
            taken_epoch: 3,
            taken_unix_ms: stat.taken_unix_ms,
            uploaded: 1,
            pending_upload_bytes: 0,
            oldest_pending_ms: 0,
        }
    );

    daemon.resize(&base, 2 * CAPACITY).unwrap();
    assert_eq!(daemon.stat(&base).unwrap().capacity, 2 * CAPACITY);
    // Its guest, which would have to agree, isn't there.
    assert_eq!(failure(daemon.resize(&base, CAPACITY)), (Code::FailedPrecondition, Errno::EBUSY));

    let forked = daemon.fork(&base, &fork).unwrap();
    assert_ne!(forked, branch);
    let stat = daemon.stat(&fork).unwrap();
    assert_eq!((stat.branch, stat.used, stat.public), (forked.id, 64 << 10, false));
    // TASK-49: all it holds, it inherited; TASK-454: the extent too.
    assert_eq!((stat.own, stat.inherited), (0, 64 << 10));
    assert_eq!((stat.own_extents, stat.inherited_extents), (0, 1));
    daemon.make_public(&base).unwrap();
    assert!(daemon.stat(&base).unwrap().public);
    assert!(!daemon.stat(&fork).unwrap().public, "a fork of a public base is its own");

    // The fork's commits in object storage, another host takes its branch, placed after this
    // one's: what it holds there.
    daemon.sync(&fork).unwrap();
    assert_eq!(daemon.create(&taken, forked, Some(2)).unwrap(), forked);
    let vm = daemon.open(&taken, CAPACITY);
    assert_eq!(vm.root().unwrap(), b"base");
    drop(vm);

    daemon.place(&base, 3).unwrap();
    daemon.delete(&taken).unwrap();
    assert_eq!(failure(daemon.stat(&taken)), (Code::NotFound, Errno::ENOENT));

    let counters = daemon.counters();
    let object_storage = counters.object_storage.expect("the daemon has a bucket");
    assert!(object_storage.requests["create"] > 0, "{object_storage:?}");
    assert!(object_storage.uploaded_bytes.values().sum::<u64>() >= 64 << 10, "{object_storage:?}");
}

/// TASK-50.5: the stores VM processes have open are listed, with their writers' takings of their
/// branches' heads, as this host's placement took them, and their uploads: synced, nothing waits.
#[test]
fn open_stores_are_listed_with_their_takings_and_uploads() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let (first, second) = (dir.path().join("first"), dir.path().join("second"));
    let before = std::time::SystemTime::now();
    let one = daemon.open(&first, CAPACITY);
    write(&one, &pattern(1), b"one");
    let two = daemon.open(&second, CAPACITY);
    daemon.sync(&first).unwrap();
    daemon.sync(&second).unwrap();
    let after = std::time::SystemTime::now();

    let listed = daemon.list();
    let paths: Vec<_> = listed.iter().map(|(path, _)| path.clone()).collect();
    let canonical = |path: &Path| path.canonicalize().unwrap();
    assert_eq!(paths, [canonical(&first), canonical(&second)]);
    let millis = |time: std::time::SystemTime| {
        time.duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
    };
    for (((_, stat), branch), extents) in
        listed.iter().zip([one.branch(), two.branch()]).zip([1, 0])
    {
        assert_eq!((stat.branch, stat.open, stat.uploading), (branch, true, true));
        // TASK-454: the extent the first wrote; none, the second.
        assert_eq!((stat.own_extents, stat.inherited_extents), (extents, 0));
        assert_eq!((stat.taken_token, stat.taken_epoch), (1, 1));
        assert!((millis(before)..=millis(after)).contains(&stat.taken_unix_ms), "{stat:?}");
        assert_eq!(stat.uploaded, stat.committed, "{stat:?}");
        assert_eq!((stat.pending_upload_bytes, stat.oldest_pending_ms), (0, 0));
    }
    assert_eq!(daemon.stat(&first).unwrap(), listed[0].1);
    drop((one, two));
}

/// TASK-454: StatStore and ListStores say how many extents a store holds, its own and those it
/// inherited, as its VM writes and frees them, open and closed, and of its fork.
#[test]
fn extents_are_counted_through_the_control_api() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let (base, fork) = (dir.path().join("base"), dir.path().join("fork"));
    let write = |vm: &cladefs::RemoteExtentStore, sequence: u64| {
        let extent = cladefs::ExtentId { branch: vm.branch(), sequence };
        vm.write(extent, 0, &[cladefs::VolatileSlice::from(&pattern(sequence)[..4096])]).unwrap();
        extent
    };
    let counts = |stat: &StatStoreResponse| (stat.own_extents, stat.inherited_extents);

    let vm = daemon.open(&base, CAPACITY);
    assert_eq!(counts(&daemon.stat(&base).unwrap()), (0, 0));
    let written: Vec<_> = (1..=3).map(|sequence| write(&vm, sequence)).collect();
    vm.commit(None).unwrap();
    assert_eq!(counts(&daemon.stat(&base).unwrap()), (3, 0));
    drop(vm);
    assert_eq!(counts(&daemon.stat(&base).unwrap()), (3, 0));

    daemon.fork(&base, &fork).unwrap();
    assert_eq!(counts(&daemon.stat(&fork).unwrap()), (0, 3));
    let vm = daemon.open(&fork, CAPACITY);
    let own = [write(&vm, 1), write(&vm, 2)];
    vm.free(written[0]).unwrap();
    vm.free(own[1]).unwrap();
    // Open, as its VM has it, committed or not.
    assert_eq!(counts(&daemon.stat(&fork).unwrap()), (1, 2));
    vm.commit(None).unwrap();
    let canonical = fork.canonicalize().unwrap();
    let listed = daemon.list().into_iter().find(|(path, _)| *path == canonical);
    assert_eq!(listed.map(|(_, stat)| counts(&stat)), Some((1, 2)));
    drop(vm);
    assert_eq!(counts(&daemon.stat(&fork).unwrap()), (1, 2));
    assert_eq!(counts(&daemon.stat(&base).unwrap()), (3, 0));
}

/// A store a VM process has open is its: the API refuses what would change it under its guest,
/// grows it, forks it live, and syncs it; and calls fail with the errnos the .proto says.
#[test]
fn open_stores_are_their_vms() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let (store, fork) = (dir.path().join("store"), dir.path().join("fork"));
    let vm = daemon.open(&store, CAPACITY);
    write(&vm, &pattern(2), b"open");

    assert!(daemon.stat(&store).unwrap().open);
    let busy = (Code::FailedPrecondition, Errno::EBUSY);
    assert_eq!(failure(daemon.delete(&store)), busy);
    assert_eq!(failure(daemon.make_public(&store)), busy);
    assert_eq!(failure(daemon.place(&store, 1)), busy);
    assert_eq!(failure(daemon.create(&store, NEW, None)), busy);
    // Grown at once, as its VM process sees.
    daemon.resize(&store, 2 * CAPACITY).unwrap();
    assert_eq!(vm.info().unwrap().capacity, 2 * CAPACITY);
    // Forked live: nothing is open on it.
    let branch = daemon.fork(&store, &fork).unwrap();
    assert_eq!(daemon.branch(&fork).unwrap(), branch);

    assert_eq!(failure(daemon.create(&fork, NEW, None)), (Code::AlreadyExists, Errno::EEXIST));
    let none = dir.path().join("none");
    assert_eq!(failure(daemon.stat(&none)), (Code::NotFound, Errno::ENOENT));
    assert_eq!(
        failure(daemon.create(Path::new(""), NEW, None)),
        (Code::InvalidArgument, Errno::EINVAL)
    );
    // Synced as its VM process goes on.
    daemon.sync(&store).unwrap();

    // Gone with its VM process, it is the API's.
    drop(vm);
    assert!(!daemon.stat(&store).unwrap().open);
    daemon.delete(&store).unwrap();
}

/// A container image imported is a store holding its files.
#[test]
fn imports_images() {
    use cladefs::import::testing::{Entry, Kind, layer};
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let image = dir.path().join("layer.tar");
    let hello = b"hello from the image\n";
    std::fs::write(&image, layer(&[Entry::new("hello", Kind::File(hello))]).unwrap()).unwrap();
    let store = dir.path().join("store");

    let branch = daemon.import(&store, &image).unwrap();
    assert_eq!(daemon.branch(&store).unwrap(), branch);
    assert_eq!(failure(daemon.import(&store, &image)), (Code::AlreadyExists, Errno::EEXIST));

    let size = futures::executor::block_on(async {
        let fs = cladefs::Filesystem::mount(cladefs::Storage::open(&store).unwrap()).await.unwrap();
        let (_, attr) = fs.root().lookup(b"hello").await.unwrap();
        fs.close().await.unwrap();
        attr.size
    });
    assert_eq!(size, hello.len() as u64);
}

/// An image that cannot be made into a store leaves none behind, so that its path takes another:
/// one whose entry's time is out of range panicked the import, which left the store half made,
/// and every import to the path after failed with `EEXIST`.
#[test]
fn imports_that_fail_leave_no_store() {
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
    let store = dir.path().join("store");

    assert_eq!(
        failure(daemon.import(&store, &out_of_range)),
        (Code::InvalidArgument, Errno::EINVAL)
    );
    assert!(!store.exists(), "the store is left half made");
    let branch = daemon.import(&store, &whole).unwrap();
    assert_eq!(daemon.branch(&store).unwrap(), branch);
}

/// A fork of a branch this host never had: the branch's store taken from object storage
/// (CreateStore of its branch, placed after the host that wrote it), then forked (ForkStore), whose
/// VM reads what the branch held, its data fetched as read.
#[test]
fn forks_of_branches_never_had_here() {
    // SAFETY: as in stores_through_the_control_api.
    unsafe {
        std::env::set_var("CLADEFS_EXTENTD_REMOTE", "memory:///elsewhere");
        std::env::set_var("CLADEFS_EXTENTD_TOKEN", "1");
    }
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let (written, taken, fork) =
        (dir.path().join("written"), dir.path().join("taken"), dir.path().join("fork"));
    // Another host's: written, synced, and gone from here.
    let vm = daemon.open(&written, CAPACITY);
    write(&vm, &pattern(7), b"elsewhere");
    drop(vm);
    daemon.sync(&written).unwrap();
    // As the VM manager keeps it: its tenant, its cell and its ID (TASK-364).
    let branch = daemon.branch(&written).unwrap();
    std::fs::remove_dir_all(&written).unwrap();

    assert_eq!(daemon.create(&taken, branch, Some(2)).unwrap(), branch);
    let forked = daemon.fork(&taken, &fork).unwrap();
    let vm = daemon.open(&fork, CAPACITY);
    assert_eq!(vm.branch(), forked.id);
    assert_eq!(vm.root().unwrap(), b"elsewhere");
    let extent = cladefs::ExtentId { branch: branch.id, sequence: 1 };
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
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let idle = dir.path().join("idle");
    daemon.create(&idle, NEW, None).unwrap();
    let count = cladefs::extentd::control::BLOCKING_THREADS + 16;
    let asked = Arc::new(AtomicUsize::new(0));
    let (vms, paths): (Vec<_>, Vec<_>) = (0..count)
        .map(|vm| {
            let path = dir.path().join(format!("vm-{vm}"));
            let store = daemon.open(&path, CAPACITY);
            // A guest that never answers.
            let asked = asked.clone();
            store.on_capacity(move |_, ask| {
                if ask {
                    asked.fetch_add(1, Ordering::Relaxed);
                }
            });
            (store, path)
        })
        .collect();

    let shrinks: Vec<_> = (paths.iter())
        .map(|path| {
            let mut client = daemon.client.clone();
            let request = ResizeStoreRequest { path: name(path), capacity: CAPACITY / 2 };
            daemon.runtime.spawn(async move { client.resize_store(request).await })
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
    assert_eq!(daemon.stat(&idle).unwrap().capacity, CAPACITY);
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
        std::env::set_var("CLADEFS_EXTENTD_REMOTE", "memory:///branches");
        std::env::set_var("CLADEFS_EXTENTD_TOKEN", "1");
    }
    let dir = tempfile::tempdir().unwrap();
    let control = dir.path().join("control.sock");
    let listener = std::os::unix::net::UnixListener::bind(&control).unwrap();
    let tenants = dir.path().join("tenants");
    std::thread::spawn(move || cladefs::extentd::serve_control(listener, tenants));
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        let channel = cladefs::extentd::control::channel(&control).await.unwrap();
        let health = HealthClient::new(channel);
        let mut watcher = health.clone();
        let status = |service: &str| {
            let mut health = health.clone();
            let request = HealthCheckRequest { service: service.to_owned() };
            async move { health.check(request).await.unwrap().into_inner().status() }
        };
        let api = "cladefs.extentd.v1.Extentd";
        assert_eq!(status("").await, ServingStatus::NotServing);
        assert_eq!(status(api).await, ServingStatus::NotServing);
        let request = HealthCheckRequest { service: String::new() };
        let mut watch = watcher.watch(request).await.unwrap().into_inner();
        let mut next = async || watch.message().await.unwrap().unwrap().status();
        assert_eq!(next().await, ServingStatus::NotServing);

        let vms = std::os::unix::net::UnixListener::bind(dir.path().join("vms.sock")).unwrap();
        std::thread::spawn(move || cladefs::extentd::serve(vms));
        assert_eq!(next().await, ServingStatus::Serving);
        assert_eq!(status(api).await, ServingStatus::Serving);

        tokio::task::spawn_blocking(cladefs::extentd::stop_serving).await.unwrap();
        assert_eq!(next().await, ServingStatus::NotServing);
        assert_eq!(status(api).await, ServingStatus::NotServing);
    });
}

/// TASK-411.7, and TASK-411's AC #3 (extentd serving a VM's store of a sealed cell through its
/// socket): stores of two cells of a tenant whose KEK the daemon's KMS (`CLADEFS_KMS`) keeps, each
/// written by its VM, read back, no file of their directories holding what they wrote in the
/// clear. `ShredCell` takes the first cell's wrapped KEK out of its object and fences its store,
/// its VM's reads failing with EACCES, the other's reading on; called again, it finds the cell
/// shredded and fences nothing more. `ForgetTenantKeys`, once the tenant's KEK is destroyed in the
/// KMS, fences the other. Closed, neither store opens again (PERMISSION_DENIED, EACCES).
#[test]
fn sealed_stores_served_and_shredded_through_the_control_api() {
    let keys = tempfile::tempdir().unwrap();
    let kms_dir = keys.path().join("kms");
    let kms = extent_keys::kms::LocalKms::open(&kms_dir).unwrap();
    kms.create(1).unwrap();
    // SAFETY: nextest runs each test in a process of its own, whose other threads don't read the
    // environment yet.
    unsafe { std::env::set_var("CLADEFS_KMS", format!("local://{}", kms_dir.display())) };
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let (ours, sibling) = (dir.path().join("ours"), dir.path().join("sibling"));
    let of_tenant = cladefs::Branch { cell: cladefs::Cell { tenant: 1, id: 0 }, id: 0 };
    let branch = daemon.create(&ours, of_tenant, None).unwrap();
    let other = daemon.create(&sibling, of_tenant, None).unwrap();
    assert_ne!(branch.cell, other.cell);
    let vms = [daemon.open(&ours, CAPACITY), daemon.open(&sibling, CAPACITY)];
    for (seed, vm) in (1..).zip(&vms) {
        write(vm, &pattern(seed), b"sealed");
    }
    let read = |vm: &cladefs::RemoteExtentStore, seed: u64| {
        let mut data = vec![0; 64 << 10];
        let extent = cladefs::ExtentId { branch: vm.branch(), sequence: 1 };
        let done = vm.read(extent, 0, &mut [cladefs::VolatileSliceMut::from(&mut data[..])]);
        done.map(|()| data == pattern(seed))
    };
    assert_eq!((read(&vms[0], 1), read(&vms[1], 2)), (Ok(true), Ok(true)));
    daemon.sync(&ours).unwrap();
    daemon.sync(&sibling).unwrap();
    // Beside the daemon's stores, which work on behind its API (TASK-463).
    for (seed, path) in (1..).zip([&ours, &sibling]) {
        let held = cladefs::testing::files_holding_beside(path, &[&pattern(seed)[..64]]);
        assert_eq!(held, Vec::<String>::new(), "files holding what their VM wrote");
    }

    let shred = |daemon: &mut Daemon| {
        let request = ShredCellRequest { tenant: 1, cell: branch.cell.id };
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

    drop(vms);
    let denied = (Code::PermissionDenied, Errno::EACCES);
    assert_eq!(failure(daemon.stat(&ours)), denied);
    assert_eq!(failure(daemon.stat(&sibling)), denied);
}
