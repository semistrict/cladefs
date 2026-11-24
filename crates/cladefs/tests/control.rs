// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-43.3: the extent daemon's control API, gRPC over a Unix socket, as the host's VM manager
//! calls it, from a client in Rust: every call, and the errors a cell a VM has open gives.
//!
//! The daemon serves on threads of this process: its control socket, and a VM socket, whose
//! connections, VM processes here, have cells open.

#![cfg(target_os = "linux")]

mod common;

use cladefs::Errno;
use cladefs::cladefsd::control::proto::*;
use common::daemon::{CAPACITY, Daemon, NEW, failure, name, write};
use common::pattern;
use std::path::Path;
use tonic::Code;

/// A cell made, written by a VM, grown, forked, made a public base, synced to object storage and
/// taken from there to a new cell directory, placed, and deleted, through the API.
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
    let (base, fork, taken) =
        (dir.path().join("base"), dir.path().join("fork"), dir.path().join("taken"));

    let cell = daemon.create(&base, NEW, None).unwrap();
    assert_ne!(cell.id, 0);
    let vm = daemon.open(&base, CAPACITY);
    assert_eq!(vm.cell(), cell.id);
    write(&vm, &pattern(1), b"base");
    drop(vm);
    let stat = daemon.wait_closed(&base);
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
            // Not replicated (TASK-537.3.1).
            replicas: 0,
            fsync: FsyncLevel::Ordered.into(),
            configuration: 0,
            degraded: false,
            members_up: 0,
            active_replicas: 0,
            // Taken unforced (TASK-681).
            forced_after: None,
        }
    );

    daemon.resize(&base, 2 * CAPACITY).unwrap();
    assert_eq!(daemon.stat(&base).unwrap().capacity, 2 * CAPACITY);
    // Its guest, which would have to agree, isn't there.
    assert_eq!(failure(daemon.resize(&base, CAPACITY)), (Code::FailedPrecondition, Errno::EBUSY));

    let forked = daemon.fork(&base, &fork).unwrap();
    assert_ne!(forked, cell);
    let stat = daemon.stat(&fork).unwrap();
    assert_eq!((stat.cell, stat.used, stat.public), (forked.id, 64 << 10, false));
    // TASK-49: all it holds, it inherited; TASK-454: the extent too.
    assert_eq!((stat.own, stat.inherited), (0, 64 << 10));
    assert_eq!((stat.own_extents, stat.inherited_extents), (0, 1));
    daemon.make_public(&base).unwrap();
    assert!(daemon.stat(&base).unwrap().public);
    assert!(!daemon.stat(&fork).unwrap().public, "a fork of a public base is its own");

    // The fork's commits in object storage, another host takes its cell, placed after this
    // one's: what it holds there.
    daemon.sync(&fork).unwrap();
    assert_eq!(daemon.create(&taken, forked, Some(2)).unwrap(), forked);
    let vm = daemon.open(&taken, CAPACITY);
    assert_eq!(vm.root().unwrap(), b"base");
    drop(vm);
    daemon.wait_closed(&taken);

    daemon.place(&base, 3).unwrap();
    daemon.delete(&taken).unwrap();
    assert_eq!(failure(daemon.stat(&taken)), (Code::NotFound, Errno::ENOENT));

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
    for (((_, stat), cell), extents) in listed.iter().zip([one.cell(), two.cell()]).zip([1, 0]) {
        assert_eq!((stat.cell, stat.open, stat.uploading), (cell, true, true));
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

/// TASK-454: StatCell and ListCells say how many extents a cell holds, its own and those it
/// inherited, as its VM writes and frees them, open and closed, and of its fork.
#[test]
fn extents_are_counted_through_the_control_api() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let (base, fork) = (dir.path().join("base"), dir.path().join("fork"));
    let write = |vm: &cladefs::CellClient, sequence: u64| {
        let extent = cladefs::ExtentId { cell: vm.cell(), sequence };
        vm.write(extent, 0, &[cladefs::VolatileSlice::from(&pattern(sequence)[..4096])]).unwrap();
        extent
    };
    let counts = |stat: &StatCellResponse| (stat.own_extents, stat.inherited_extents);

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

/// A cell a VM process has open is its: the API refuses what would change it under its guest,
/// grows it, forks it live, and syncs it; and calls fail with the errnos the .proto says.
#[test]
fn open_cells_are_their_vms() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let (cell_dir, fork) = (dir.path().join("store"), dir.path().join("fork"));
    let vm = daemon.open(&cell_dir, CAPACITY);
    write(&vm, &pattern(2), b"open");

    assert!(daemon.stat(&cell_dir).unwrap().open);
    let busy = (Code::FailedPrecondition, Errno::EBUSY);
    assert_eq!(failure(daemon.delete(&cell_dir)), busy);
    assert_eq!(failure(daemon.make_public(&cell_dir)), busy);
    assert_eq!(failure(daemon.place(&cell_dir, 1)), busy);
    assert_eq!(failure(daemon.create(&cell_dir, NEW, None)), busy);
    // Grown at once, as its VM process sees.
    daemon.resize(&cell_dir, 2 * CAPACITY).unwrap();
    assert_eq!(vm.info().unwrap().capacity, 2 * CAPACITY);
    // Forked live: nothing is open on it.
    let cell = daemon.fork(&cell_dir, &fork).unwrap();
    assert_eq!(daemon.cell(&fork).unwrap(), cell);

    assert_eq!(failure(daemon.create(&fork, NEW, None)), (Code::AlreadyExists, Errno::EEXIST));
    let none = dir.path().join("none");
    assert_eq!(failure(daemon.stat(&none)), (Code::NotFound, Errno::ENOENT));
    assert_eq!(
        failure(daemon.create(Path::new(""), NEW, None)),
        (Code::InvalidArgument, Errno::EINVAL)
    );
    // Synced as its VM process goes on.
    daemon.sync(&cell_dir).unwrap();

    // Gone with its VM process, once the daemon finds it gone, it is the API's.
    drop(vm);
    daemon.wait_closed(&cell_dir);
    daemon.delete(&cell_dir).unwrap();
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
    let cell_dir = dir.path().join("store");

    let cell = daemon.import(&cell_dir, &image).unwrap();
    assert_eq!(daemon.cell(&cell_dir).unwrap(), cell);
    assert_eq!(failure(daemon.import(&cell_dir, &image)), (Code::AlreadyExists, Errno::EEXIST));

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

/// An image that cannot be made into a cell leaves none behind, so that its path takes another:
/// one whose entry's time is out of range panicked the import, which left the cell half made,
/// and every import to the path after failed with `EEXIST`.
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
    let cell_dir = dir.path().join("store");

    assert_eq!(
        failure(daemon.import(&cell_dir, &out_of_range)),
        (Code::InvalidArgument, Errno::EINVAL)
    );
    assert!(!cell_dir.exists(), "the cell_dir is left half made");
    let cell = daemon.import(&cell_dir, &whole).unwrap();
    assert_eq!(daemon.cell(&cell_dir).unwrap(), cell);
}

/// An import its caller gave up on runs to its end, its path held until then (board 5599, board
/// 5384): stopped at the await its caller left it in, it let go of its path while its blocking
/// work went on, and a CreateCell of the path made a cell there beside the import's.
#[test]
fn imports_given_up_on_hold_their_paths_to_their_end() {
    use cladefs::import::testing::{Entry, Kind, layer};
    use std::sync::{Mutex, mpsc};
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let image = dir.path().join("layer.tar");
    let hello = b"hello from the image\n";
    std::fs::write(&image, layer(&[Entry::new("hello", Kind::File(hello))]).unwrap()).unwrap();
    let cell_dir = dir.path().join("store");

    // The import held under way, its path held, saying so, until let go.
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
        path: name(&cell_dir),
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
    // The path is the import's still: a CreateCell of it waits its turn, and gives up.
    let busy = failure(daemon.create(&cell_dir, NEW, None));
    assert_eq!(busy, (Code::FailedPrecondition, Errno::EBUSY));

    // Let go, the import ends, the cell the image's.
    fail::remove("cladefsd::import");
    go.send(()).unwrap();
    assert_eq!(failure(daemon.import(&cell_dir, &image)), (Code::AlreadyExists, Errno::EEXIST));
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
    let (written, taken, fork) =
        (dir.path().join("written"), dir.path().join("taken"), dir.path().join("fork"));
    // Another host's: written, synced, and gone from here.
    let vm = daemon.open(&written, CAPACITY);
    write(&vm, &pattern(7), b"elsewhere");
    drop(vm);
    daemon.sync(&written).unwrap();
    // As the VM manager keeps it: its tenant, its cell and its ID (TASK-364).
    let cell = daemon.cell(&written).unwrap();
    std::fs::remove_dir_all(&written).unwrap();

    assert_eq!(daemon.create(&taken, cell, Some(2)).unwrap(), cell);
    let forked = daemon.fork(&taken, &fork).unwrap();
    let vm = daemon.open(&fork, CAPACITY);
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
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let idle = dir.path().join("idle");
    daemon.create(&idle, NEW, None).unwrap();
    let count = cladefs::cladefsd::control::BLOCKING_THREADS + 16;
    let asked = Arc::new(AtomicUsize::new(0));
    let (vms, paths): (Vec<_>, Vec<_>) = (0..count)
        .map(|vm| {
            let path = dir.path().join(format!("vm-{vm}"));
            let client = daemon.open(&path, CAPACITY);
            // A guest that never answers.
            let asked = asked.clone();
            client.on_capacity(move |_, ask| {
                if ask {
                    asked.fetch_add(1, Ordering::Relaxed);
                }
            });
            (client, path)
        })
        .collect();

    let shrinks: Vec<_> = (paths.iter())
        .map(|path| {
            let mut client = daemon.client.clone();
            let request = ResizeCellRequest { path: name(path), capacity: CAPACITY / 2 };
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
    let (ours, sibling) = (dir.path().join("ours"), dir.path().join("sibling"));
    let of_tenant = cladefs::CellId { tenant: 1, id: 0 };
    let cell = daemon.create(&ours, of_tenant, None).unwrap();
    let other = daemon.create(&sibling, of_tenant, None).unwrap();
    assert_ne!(cell, other);
    let vms = [daemon.open(&ours, CAPACITY), daemon.open(&sibling, CAPACITY)];
    for (seed, vm) in (1..).zip(&vms) {
        write(vm, &pattern(seed), b"encrypted");
    }
    let read = |vm: &cladefs::CellClient, seed: u64| {
        let mut data = vec![0; 64 << 10];
        let extent = cladefs::ExtentId { cell: vm.cell(), sequence: 1 };
        let done = vm.read(extent, 0, &mut [cladefs::VolatileSliceMut::from(&mut data[..])]);
        done.map(|()| data == pattern(seed))
    };
    assert_eq!((read(&vms[0], 1), read(&vms[1], 2)), (Ok(true), Ok(true)));
    daemon.sync(&ours).unwrap();
    daemon.sync(&sibling).unwrap();
    // Beside the daemon's cells, which work on behind its API (TASK-463).
    for (seed, path) in (1..).zip([&ours, &sibling]) {
        let held = cladefs::testing::files_holding_beside(path, &[&pattern(seed)[..64]]);
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
    assert_eq!(failure(daemon.stat_once_closed(&ours)), denied);
    assert_eq!(failure(daemon.stat_once_closed(&sibling)), denied);
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
    let (ours, theirs) = (dir.path().join("ours"), dir.path().join("theirs"));
    let of = |tenant| cladefs::CellId { tenant, id: 0 };
    daemon.create(&ours, of(1), None).unwrap();
    daemon.create(&theirs, of(2), None).unwrap();
    let vms = [daemon.open(&ours, CAPACITY), daemon.open(&theirs, CAPACITY)];
    for (seed, vm) in (1..).zip(&vms) {
        write(vm, &pattern(seed), b"encrypted");
    }
    let read = |vm: &cladefs::CellClient, seed: u64| {
        let mut data = vec![0; 64 << 10];
        let extent = cladefs::ExtentId { cell: vm.cell(), sequence: 1 };
        let done = vm.read(extent, 0, &mut [cladefs::VolatileSliceMut::from(&mut data[..])]);
        done.map(|()| data == pattern(seed))
    };
    assert_eq!((read(&vms[0], 1), read(&vms[1], 2)), (Ok(true), Ok(true)));
    daemon.sync(&ours).unwrap();
    daemon.sync(&theirs).unwrap();
    for (seed, path) in (1..).zip([&ours, &theirs]) {
        let mut paths = vec![path.to_path_buf()];
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
    assert_eq!(failure(daemon.stat_once_closed(&ours)), (Code::PermissionDenied, Errno::EACCES));
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
    let of = |tenant| cladefs::CellId { tenant, id: 0 };
    let reload = |daemon: &mut Daemon| {
        let reloaded = daemon.runtime.block_on(daemon.client.reload_kms(ReloadKmsRequest {}));
        reloaded.map(tonic::Response::into_inner)
    };
    let seven = base32_id::Short(7).to_string();
    daemon.create(&dir.path().join("one"), of(1), None).unwrap();
    let early = daemon.create(&dir.path().join("early"), of(7), None);
    assert_eq!(failure(early).0, Code::PermissionDenied);

    std::fs::write(&keys, format!("# tenants with keys of their own\n{seven} byok-7\n")).unwrap();
    let reloaded = reload(&mut daemon).unwrap();
    assert_eq!(reloaded, ReloadKmsResponse { renamed: vec![7], master_keys: vec![] });
    let byok = dir.path().join("byok");
    daemon.create(&byok, of(7), None).unwrap();
    let vm = daemon.open(&byok, CAPACITY);
    write(&vm, &pattern(7), b"byok");
    drop(vm);
    daemon.sync(&byok).unwrap();
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
    daemon.create(&dir.path().join("still"), of(7), None).unwrap();
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
    let of = |tenant| cladefs::CellId { tenant, id: 0 };
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
    daemon.create(&dir.path().join("one"), of(1), None).unwrap();

    std::fs::write(&file, format!("{b}\n{a}\n")).unwrap();
    assert_eq!(reload(&mut daemon).unwrap(), changed(&[&b], &[]));
    daemon.create(&dir.path().join("two"), of(2), None).unwrap();

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
    daemon.create(&dir.path().join("three"), of(3), None).unwrap();

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
    daemon.create(&dir.path().join("four"), of(4), None).unwrap();
}

/// TASK-537.3.1 (doc-21): CreateCell gives a cell its replication, its f and what its fsync waits
/// for, in its head, which StatCell reports of the open cell; a taking that asks for nothing, on
/// any host, keeps them, and one that asks for another changes them. Asks no f can satisfy are
/// refused, and an f of 1 or more by a daemon with no peers (TASK-537.3.3).
#[test]
fn cells_are_made_replicated_through_the_control_api() {
    // SAFETY: as in cells_through_the_control_api.
    unsafe {
        std::env::set_var("CLADEFSD_BUCKET", "memory:///replicated");
        std::env::set_var("CLADEFSD_TOKEN", "1");
    }
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let request =
        |path: &Path, cell: u64, replicas: Option<u32>, fsync: FsyncLevel| CreateCellRequest {
            path: name(path),
            capacity: CAPACITY,
            cell,
            replicas,
            fsync: fsync.into(),
            ..CreateCellRequest::default()
        };
    let create = |daemon: &mut Daemon, request: CreateCellRequest| {
        daemon.runtime.block_on(daemon.client.create_cell(request)).map(|made| made.into_inner())
    };
    let replicated = |daemon: &mut Daemon, path: &Path| {
        let client = daemon.open(path, CAPACITY);
        let stat = daemon.stat(path).unwrap();
        drop(client);
        (stat.replicas, stat.fsync(), stat.configuration)
    };

    // This daemon has no peers, nor a replica directory: an f of 1 or more is refused before
    // anything is made (TASK-537.3.3's fail-hard rule), and the head keeps what an f of 0 asks.
    let first = dir.path().join("first");
    let refused = create(&mut daemon, request(&first, 0, Some(1), FsyncLevel::Replicated));
    // Board 8583: what the daemon said of it, as a client of the API, cladefsctl's, says it, its
    // errno with it; an errno said alone, not said twice.
    let said = cladefs::cladefsd::control::failed("making the cell".into(), refused.unwrap_err());
    assert_eq!(
        format!("{said:#}"),
        "making the cell: a replicated cell needs the daemon's peers and replica directory \
         (CLADEFSD_PEER_ADDRESS, CLADEFSD_PEERS, CLADEFSD_REPLICAS): ENOTSUP (errno 95)"
    );
    assert_eq!(said.errno(), Some(Errno::ENOTSUP));
    let bare = create(&mut daemon, request(&first, 0, Some(4), FsyncLevel::Ordered));
    let said = cladefs::cladefsd::control::failed("making the cell".into(), bare.unwrap_err());
    assert_eq!(format!("{said:#}"), "making the cell: EINVAL (errno 22)");
    assert_eq!(said.errno(), Some(Errno::EINVAL));
    assert!(!first.exists());
    // Deprecated FSYNC_LEVEL_LOCAL maps to FSYNC_LEVEL_ORDERED with a warning for backwards
    // compatibility.
    #[allow(deprecated)]
    let made = create(&mut daemon, request(&first, 0, Some(0), FsyncLevel::Local)).unwrap();
    assert_eq!(replicated(&mut daemon, &first), (0, FsyncLevel::Ordered, 0));
    daemon.sync(&first).unwrap();

    // Taken into another directory, asking for nothing: kept. Then asking for another: changed.
    let second = dir.path().join("second");
    create(&mut daemon, request(&second, made.cell, None, FsyncLevel::Ordered)).unwrap();
    assert_eq!(replicated(&mut daemon, &second), (0, FsyncLevel::Ordered, 0));
    daemon.sync(&second).unwrap();
    let third = dir.path().join("third");
    create(&mut daemon, request(&third, made.cell, Some(0), FsyncLevel::Ordered)).unwrap();
    assert_eq!(replicated(&mut daemon, &third), (0, FsyncLevel::Ordered, 0));

    // A cell made asking for nothing is not replicated.
    let plain = dir.path().join("plain");
    create(&mut daemon, request(&plain, 0, None, FsyncLevel::Ordered)).unwrap();
    assert_eq!(replicated(&mut daemon, &plain), (0, FsyncLevel::Ordered, 0));

    // No f past 3, no fsync level without replicas, no fsync waiting for copies an f of 0 never
    // makes: refused, nothing made.
    #[allow(deprecated)]
    let refused_cases = [
        (Some(4), FsyncLevel::Ordered),
        (None, FsyncLevel::Local),
        (None, FsyncLevel::Replicated),
        (Some(0), FsyncLevel::Replicated),
    ];
    for (replicas, fsync) in refused_cases {
        let path = dir.path().join("refused");
        let refused = create(&mut daemon, request(&path, 0, replicas, fsync));
        assert_eq!(
            failure(refused),
            (Code::InvalidArgument, Errno::EINVAL),
            "{replicas:?} {fsync:?}"
        );
        assert!(!path.exists());
    }
}

/// What a guest of today says of its stats, as its Fxfs and its driver write them: its device's
/// and its window's requests timed (a count, nanoseconds, a histogram of 24 buckets each), and the
/// rest of its counters, some 50 lines and 12 histograms in all.
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
fn stats_of_many_vms_are_taken_and_bounded() {
    use std::sync::Arc;
    const VMS: usize = 120;
    // Each VM process's cell and connection hold files open, past the usual 1,024 (board 8595).
    cladefs::cladefsd::raise_open_files_limit().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(dir.path());
    let (today, full) = (todays_guest(), full_guest());
    let vms: Vec<Arc<cladefs::CellClient>> = (0..=VMS)
        .map(|n| {
            let vm = Arc::new(daemon.open(&dir.path().join(format!("vm{n:03}")), CAPACITY));
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
    let client = cladefs::testing::Control::connect(dir.path().join("control.sock"));
    let client = client.unwrap();
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
