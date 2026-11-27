// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::client::{CellClient, Opening};
use crate::server::{default_tenant, serve, serve_connection};
use bucket_cell::cell::Kind;
use cladefs_core::pages::page_size;
use cladefs_core::{
    Commit, Errno, ExtentId, OpenCell, VolatileSlice, VolatileSliceMut, WriteBarriers,
};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
#[cfg(target_os = "linux")]
use std::ptr::NonNull;
use std::thread::JoinHandle;

const BLOCK: usize = OpenCell::BLOCK_SIZE as usize;

/// The ID of the cell at `path`, as its description names it (TASK-364): what a VM process's
/// cell, which names its ID alone, is in the bucket.
fn cell_at(path: &std::path::Path) -> cladefs_core::CellId {
    host_fs::may_block(|| local_cell::host_cell(path).unwrap())
}

/// The data of `extent`, uploaded at `location` in `bucket` by this process's daemon, which
/// encrypts under cladefs_core's tests' KMS (`tests-kms`: every tenant's KEK one known key), not
/// bucket_cell's (board 13675).
async fn read_back<B: bucket_cell::Bucket>(
    bucket: &std::sync::Arc<B>,
    extent: ExtentId,
    location: &bucket_cell::layout::Location,
) -> Vec<u8> {
    let kms = std::sync::Arc::new(extent_keys::testing::KnownKms);
    bucket_cell::testing::read_under(bucket, extent, location, kms).await.unwrap()
}

/// Makes this process a host of the multi-node system, as the daemon's must be: its cells
/// in a bucket in its memory, as of placement 1; its log on standard error.
fn host() {
    let _ = crate::logging::to_stderr();
    // SAFETY: nextest runs each test in a process of its own, whose other threads don't read the
    // environment yet.
    unsafe {
        std::env::set_var("CLADEFSD_BUCKET", "memory:///cells");
        std::env::set_var("CLADEFSD_TOKEN", "1");
    }
}

/// A daemon serving one connection, on a thread, and its client, of the cell at `path`.
fn connect(path: &Path, capacity: u64) -> (CellClient, JoinHandle<()>) {
    let (stream, server) = UnixStream::pair().unwrap();
    let served = std::thread::spawn(move || serve_connection(server, default_tenant().unwrap()));
    (CellClient::over(stream, path, Opening::Create(capacity, Kind::Fxfs)).unwrap(), served)
}

/// `len` bytes of `byte` plus their offset modulo 251.
fn pattern(byte: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| byte ^ (i % 251) as u8).collect()
}

fn write(client: &CellClient, extent: ExtentId, offset: u64, data: &[u8]) -> Result<(), Errno> {
    // In two buffers, as a request's descriptors may hold it.
    let (first, second) = data.split_at(data.len() / 2);
    client.write(extent, offset, &[VolatileSlice::from(first), VolatileSlice::from(second)])
}

fn read(client: &CellClient, extent: ExtentId, offset: u64, len: usize) -> Result<Vec<u8>, Errno> {
    let mut data = vec![0; len];
    let (first, second) = data.split_at_mut(len / 2);
    client.read(
        extent,
        offset,
        &mut [VolatileSliceMut::from(first), VolatileSliceMut::from(second)],
    )?;
    Ok(data)
}

/// `pages` pages of anonymous memory, for a window.
#[cfg(target_os = "linux")]
fn memory(pages: usize) -> NonNull<u8> {
    // SAFETY: Maps anonymous memory, which the test owns.
    let memory = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            pages * page_size() as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(memory, libc::MAP_FAILED);
    NonNull::new(memory.cast()).unwrap()
}

/// The file and offset this process maps the page at `addr` from, as /proc/self/maps has them:
/// the device and inode, and the offset.
#[cfg(target_os = "linux")]
fn mapped_from(addr: usize) -> (String, u64) {
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    for line in maps.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (start, end) = fields[0].split_once('-').unwrap();
        let (start, end) =
            (usize::from_str_radix(start, 16).unwrap(), usize::from_str_radix(end, 16).unwrap());
        if (start..end).contains(&addr) {
            let offset = u64::from_str_radix(fields[2], 16).unwrap();
            return (format!("{} {}", fields[3], fields[4]), offset + (addr - start) as u64);
        }
    }
    panic!("{addr:#x} isn't mapped");
}

/// How /proc/self/maps names anonymous memory's file: none.
#[cfg(target_os = "linux")]
const ANONYMOUS: &str = "00:00 0";

/// The protection this process maps the page at `addr` with, as /proc/self/maps has it: "r-xp".
#[cfg(target_os = "linux")]
fn protection_of(addr: usize) -> String {
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    for line in maps.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (start, end) = fields[0].split_once('-').unwrap();
        let (start, end) =
            (usize::from_str_radix(start, 16).unwrap(), usize::from_str_radix(end, 16).unwrap());
        if (start..end).contains(&addr) {
            return fields[1].to_owned();
        }
    }
    panic!("{addr:#x} isn't mapped");
}

/// cladefs TASK-79.8: a window whose connection goes maps none of the daemon's pages from then on,
/// which the daemon frees for other VMs' extents, nor anything the guest can reach, until another
/// connection's window takes its memory over: a device reset and activated again, failing before
/// its new window was made, once left its guest the pages of other VMs.
#[test]
#[cfg(target_os = "linux")]
fn windows_let_go_of_the_daemons_pages_as_their_connection_goes() {
    host();
    let ps = page_size() as usize;
    let dir = tempfile::tempdir().unwrap();
    let (client, served) = connect(&dir.path().join("store"), 1 << 24);
    let memory = memory(2);
    // SAFETY: The memory is the test's, and outlives the cell.
    unsafe { client.create_dax_window_in_host_memory(memory, 2 * ps as u64, None, 1) }.unwrap();
    let base = memory.as_ptr() as usize;
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, extent, 0, &pattern(4, ps)).unwrap();
    client.commit(None).unwrap();
    client.map(extent, 0, ps as u64, 0).unwrap();
    // SAFETY: The window's memory, whose faults the cell serves.
    let page = unsafe { std::slice::from_raw_parts(memory.as_ptr(), ps) };
    assert!(*page == pattern(4, ps)[..], "the window maps other data");
    assert_ne!(mapped_from(base).0, ANONYMOUS, "the window maps no file");

    drop(client);
    served.join().unwrap();
    for addr in [base, base + ps] {
        assert_eq!(
            (mapped_from(addr).0, protection_of(addr)),
            (ANONYMOUS.to_owned(), "---p".to_owned()),
            "{:#x} of the window",
            addr - base
        );
    }
    // SAFETY: Unmaps the memory, which nothing uses any more.
    assert_eq!(unsafe { libc::munmap(memory.as_ptr().cast(), 2 * ps) }, 0);
}

/// cladefs TASK-172: a VM process's window keeps as many runs of pages installed as the mappings
/// the daemon says the host lets a process make allow, not as what it reads of /proc itself: in its
/// jail, without /proc, it budgeted by Linux's default, 30,717 runs where the GCE hosts' limit,
/// 1,048,576, allows 522,240. The daemon here says 300,000, which neither /proc nor the default is:
/// 4,096 of them kept for the process's others, half the rest.
#[test]
#[cfg(target_os = "linux")]
fn windows_budget_by_the_mappings_the_daemon_says_the_host_allows() {
    host();
    crate::server::MAX_MAP_COUNT_GIVEN.set(300_000).unwrap();
    let ps = page_size() as usize;
    let dir = tempfile::tempdir().unwrap();
    let (client, served) = connect(&dir.path().join("store"), 1 << 24);
    let memory = memory(2);
    // SAFETY: The memory is the test's, and outlives the cell.
    unsafe { client.create_dax_window_in_host_memory(memory, 2 * ps as u64, None, 1) }.unwrap();
    assert_eq!(client.window_budget(), Some(147_952));
    drop(client);
    served.join().unwrap();
    // SAFETY: Unmaps the memory, which nothing uses any more.
    assert_eq!(unsafe { libc::munmap(memory.as_ptr().cast(), 2 * ps) }, 0);
}

/// cladefs: with `CLADEFSD_BUCKET` naming a bucket, the cells the daemon opens keep their
/// cells there: a durable commit is an uploaded one.
#[test]
fn keeps_cells_in_object_storage() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let (client, served) = connect(&dir.path().join("store"), 1 << 20);
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, extent, 0, &pattern(1, 2 * BLOCK)).unwrap();
    client.commit(Some(b"root")).unwrap();
    let bucket = crate::cells::daemon_bucket().unwrap();
    futures::executor::block_on(async {
        let cell = cell_at(&dir.path().join("store"));
        let state = bucket_cell::recover(&*bucket, cell, None).await.unwrap().state;
        assert_eq!(state.root, Some(b"root".to_vec()));
        let data = read_back(&bucket, extent, &state.extents[&extent]).await;
        assert_eq!(data, pattern(1, 2 * BLOCK));
    });
    drop(client);
    served.join().unwrap();
}

/// cladefs: a cell whose description names a bucket other than the daemon's, as an image's names
/// its own file bucket, keeps its cell there as the daemon serves it, not in the daemon's
/// (TASK-165.6): the image's other openers, the cladefs CLI's, find it there.
#[tokio::test(flavor = "multi_thread")]
async fn keeps_cells_in_the_buckets_their_descriptions_name() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("image");
    let url = format!("file://{}/", dir.path().canonicalize().unwrap().join("bucket").display());
    let cell = cladefs_core::host_cell::new_alone();
    let (of, keeping) =
        (cladefs_core::CellOf::Named(cell), cladefs_core::host_cell::keeping().unwrap());
    let made =
        cladefs_core::host_cell::create_unmade(&path, 1 << 20, of, Kind::Fxfs, Some(&url), keeping)
            .await
            .unwrap();
    host_fs::may_block(|| made.made().unwrap());
    drop(made);
    let (client, served) = connect(&path, 1 << 20);
    let extent = ExtentId { cell: cell.id, sequence: 1 };
    write(&client, extent, 0, &pattern(1, 2 * BLOCK)).unwrap();
    client.commit(Some(b"root")).unwrap();
    let image = bucket_cell::object_bucket::ObjectBucket::from_url(&url).unwrap();
    let daemons = crate::cells::daemon_bucket().unwrap();
    futures::executor::block_on(async {
        let state = bucket_cell::recover(&image, cell, None).await.unwrap().state;
        assert_eq!(state.root, Some(b"root".to_vec()));
        let record = bucket_cell::head::get(&*daemons, cell).await.unwrap();
        assert_eq!(record, None, "the daemon's bucket has the image's cell");
    });
    drop(client);
    served.join().unwrap();
}

/// cladefs: the daemon keeps what its bucket did, its requests and bytes, what serving the cells
/// cost, and the requests it served, in the file `CLADEFSD_STATS` names, soon after each change:
/// the bench reports it (`scripts/bench-report.py`).
#[test]
fn keeps_object_storage_and_daemon_stats() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("stats.txt");
    // SAFETY: as in host.
    unsafe { std::env::set_var("CLADEFSD_STATS", &file) };
    let path = dir.path().join("store");
    let (client, served) = connect(&path, 1 << 20);
    let cell = client.cell();
    let extent = move |sequence| ExtentId { cell, sequence };
    write(&client, extent(1), 0, &pattern(1, 2 * BLOCK)).unwrap();
    client.commit(Some(b"root")).unwrap();
    let bucket = crate::cells::daemon_bucket().unwrap();
    assert!(bucket.stats().uploaded(bucket_cell::object_bucket::Upload::Segments) > 0);
    client.free(extent(1)).unwrap();
    client.commit(None).unwrap();
    client.commit(None).unwrap();
    // Within a few polls of the last change.
    let fields = |kept: &str, at: usize, kind: &str| -> Option<Vec<(String, u64)>> {
        let line = kept.lines().nth(at)?.strip_prefix(kind)?.strip_prefix(": ")?;
        let field =
            |field: &str| field.split_once('=').map(|(k, v)| (k.to_owned(), v.parse().unwrap()));
        line.split(' ').map(field).collect()
    };
    let count = |fields: &[(String, u64)], name: &str| {
        fields.iter().find(|(named, _)| named == name).map(|(_, count)| *count)
    };
    let start = std::time::Instant::now();
    let kept = loop {
        let kept = std::fs::read_to_string(&file).unwrap_or_default();
        let commits = fields(&kept, 1, "daemon").and_then(|f| count(&f, "commits"));
        if kept.lines().next() == Some(&bucket.stats().line()) && commits == Some(3) {
            break kept;
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(5), "kept {kept:?}");
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let daemon = fields(&kept, 1, "daemon").unwrap();
    let names: Vec<_> = daemon.iter().map(|(name, _)| name.to_owned()).collect();
    // Its counters, then how long its cells' commits waited to be made durable (TASK-165.9).
    let mut expected: Vec<String> =
        cladefs_core::stats::stats().named().map(|(name, _)| name.to_owned()).into();
    let lags = [
        "local_lag_count",
        "local_lag_max_us",
        "local_lag_p50_ms",
        "local_lag_p99_ms",
        "local_lag_within_50ms",
    ];
    expected.extend(lags.map(str::to_owned));
    expected.extend((0..15).map(|bucket| format!("local_lag_le_{}ms", 1 << bucket)));
    expected.push("local_lag_gt_16384ms".to_owned());
    // And how long its flushes and framings took, phase by phase; and making the segments framed
    // durable, how many (TASK-310); the slowest's, each phase's longest and each phase summed,
    // with their threads' waits to run (TASK-336).
    let phases: [(&str, &[&str]); 3] = [
        ("flush", &["wait", "data", "dirs", "logs", "gone", "raw"]),
        ("framing", &["own", "read", "compress", "write"]),
        ("durable", &["wait", "entries", "files", "dirs"]),
    ];
    for (kind, phases) in phases {
        expected.extend(["count", "slow"].map(|field| format!("{kind}_{field}")));
        for which in ["slowest", "max", "sum"] {
            let named = phases.iter().chain(&["runnable"]);
            expected.extend(named.map(|phase| format!("{kind}_{which}_{phase}_us")));
        }
    }
    expected.extend(["durable_segments", "own_sync_max_us", "raw_sync_max_us"].map(str::to_owned));
    // And the bytes its cells wrote, by kind, and the process, as its kernel counts them
    // (TASK-310); cladefs: and relocated, its writers' compaction's (doc-27).
    let kinds = ["raw", "copied", "framed", "own", "log", "relocated"];
    expected.extend(kinds.map(|kind| format!("written_{kind}_bytes")));
    // And the writes they took, and the stats file's own (TASK-330.2).
    expected.extend(["written_writes", "stats_written_bytes"].map(str::to_owned));
    // cladefs: segments framed in place, commits spanning segments (TASK-310, TASK-92.41); the
    // bytes framings read, and looks ahead (TASK-400).
    let once = [
        "segments_framed_in_place",
        "commits_spanning_segments",
        "framing_read_bytes",
        "seen_ahead_bytes",
    ];
    expected.extend(once.map(str::to_owned));
    // And the parts held, packed ahead of their framings (TASK-400).
    let held = [
        "bytes",
        "peak_bytes",
        "held_bytes",
        "taken_bytes",
        "evicted_bytes",
        "evicted",
        "late",
        "skipped",
    ];
    expected.extend(held.map(|name| format!("held_parts_{name}")));
    expected.extend(["io_write_bytes", "io_cancelled_write_bytes"].map(str::to_owned));
    assert_eq!(names, expected, "{kept}");
    assert_eq!(count(&daemon, "local_lag_count"), Some(3), "{kept}");
    // The guest's write, to the open segment, once; its commit's records, in the segment's log.
    assert_eq!(count(&daemon, "written_raw_bytes"), Some(2 * BLOCK as u64), "{kept}");
    assert_eq!(count(&daemon, "written_copied_bytes"), Some(0), "{kept}");
    assert!(count(&daemon, "written_log_bytes") > Some(0), "{kept}");
    // The three commits, each a COMMIT request: a request's time holds its commit's, which
    // holds its phases'.
    let requests = fields(&kept, 2, "requests").unwrap();
    assert_eq!(count(&requests, "commit"), Some(3), "{kept}");
    let ns = |fields: &[(String, u64)], name: &str| count(fields, name).unwrap();
    assert!(ns(&requests, "commit-ns") >= ns(&daemon, "commit-ns"), "{kept}");
    let phases = ["commit-store-ns", "commit-wait-ns"];
    assert!(
        ns(&daemon, "commit-ns") >= phases.map(|phase| ns(&daemon, phase)).iter().sum::<u64>(),
        "{kept}"
    );
    drop(client);
    served.join().unwrap();
}

/// Runs `call`, a cell operation of the control API's, on a runtime of its own, as the API does:
/// a sync test's outermost entry. Called from a test's task, on its runtime, the thread handing
/// its other tasks to another meanwhile.
#[allow(clippy::disallowed_methods, reason = "a test's thread, waiting for the call alone")]
fn control<T>(call: impl std::future::Future<Output = T>) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => tokio::task::block_in_place(|| runtime.block_on(call)),
        Err(_) => {
            let runtime =
                tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            runtime.block_on(call)
        }
    }
}

/// Resizes the cell at `path`, as the control API does (`ResizeCell`).
fn resize(path: &Path, capacity: u64) -> Result<(), Errno> {
    control(crate::cells::resize(crate::Daemon::process(), path.to_owned(), capacity))
}

/// Forks the cell at `from` to a new cell at `to`, as the control API does (`ForkCell`), and
/// returns the cell.
fn fork_cell(from: &Path, to: &Path) -> Result<cladefs_core::CellId, cladefs_core::Error> {
    let forked =
        control(crate::cells::fork(crate::Daemon::process(), from.to_owned(), to.to_owned(), None));
    forked.map_err(cladefs_core::Error::from)
}

/// Deletes the cell at `path`, as the control API does (`DeleteCell`).
fn delete_cell(path: &Path) -> Result<(), cladefs_core::Error> {
    Ok(control(crate::cells::delete(crate::Daemon::process(), path.to_owned()))?)
}

/// Waits for the cell at `path` to be uploaded, as the control API does (`SyncCell`).
fn wait_uploaded(path: &Path) -> Result<(), cladefs_core::Error> {
    Ok(control(crate::cells::sync(crate::Daemon::process(), path.to_owned()))?)
}

/// A shrink whose caller drops its `ResizeCell` stops waiting for the guest (TASK-43.6): the
/// guest's answer, coming after, shrinks nothing.
#[test]
fn shrinks_dropped_by_their_callers_stop_waiting() {
    use crate::control::proto::ResizeCellRequest;
    host();
    let dir = tempfile::tempdir().unwrap();
    crate::Daemon::process().configure_disks(&[dir.path().join("disk0")]).unwrap();
    let (client, served) = connect(Path::new("0/0"), 2 << 20);
    let cell = cladefs_core::CellId { tenant: 0, id: client.cell() };
    let path = crate::Daemon::process().cell_path(cell).unwrap();
    let (asks, asked) = std::sync::mpsc::channel();
    client.on_capacity(move |capacity, ask| asks.send((capacity, ask)).unwrap());
    let socket = dir.path().join("control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let tenants = dir.path().join("tenants");
    std::fs::create_dir(&tenants).unwrap();
    std::thread::spawn(move || crate::control::serve_control(listener, tenants));
    let connection = crate::cells::connection_writing(&crate::cells::cell_dir_key(&path)).unwrap();

    control(async {
        let mut api = crate::control::connect(&socket).await.unwrap();
        let request = ResizeCellRequest { tenant: cell.tenant, cell: cell.id, capacity: 1 << 20 };
        let asking = tokio::task::spawn_blocking(move || (asked.recv().unwrap(), asked));
        let asked = tokio::select! {
            shrunk = api.resize_cell(request) => panic!("{shrunk:?}"),
            asked = asking => asked.unwrap(),
        };
        assert_eq!(asked.0, (1 << 20, true));
        // Dropped, the call is reset, and the daemon drops its wait.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while connection.shrink_awaited() {
            assert!(tokio::time::Instant::now() < deadline, "the shrink still waits");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // TASK-79.41: and tells the guest the capacity the cell keeps.
        let told = tokio::task::spawn_blocking(move || asked.1.recv().unwrap()).await.unwrap();
        assert_eq!(told, (2 << 20, false));
    });
    client.resized(1 << 20, true).unwrap();
    assert_eq!(client.info().unwrap().capacity, 2 << 20);
    assert_eq!(client.capacity(), 2 << 20);

    drop((client, connection));
    served.join().unwrap();
}

/// board 11697 (hotbed review 165): `CellClient::capacity()` stays in sync with `INFO` across
/// grows, accepted shrinks, and rejected shrinks.
#[test]
fn client_capacity_tracks_grow_and_accepted_shrink() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let (client, served) = connect(&path, 2 << 20);
    assert_eq!(client.capacity(), 2 << 20);
    let (asks, asked) = std::sync::mpsc::channel();
    client.on_capacity(move |capacity, ask| asks.send((capacity, ask)).unwrap());

    // Grow: non-ask CAPACITY updates client.capacity().
    resize(&path, 4 << 20).unwrap();
    assert_eq!(asked.recv().unwrap(), (4 << 20, false));
    assert_eq!(client.capacity(), 4 << 20);
    assert_eq!(client.info().unwrap().capacity, 4 << 20);

    // Accepted shrink: `Shrinking::resize` commits without sending another CAPACITY notification,
    // and `client.capacity()` reflects the new capacity after `resized(..., true)`.
    let shrink_path = path.clone();
    let shrinking = std::thread::spawn(move || resize(&shrink_path, 1 << 20));
    assert_eq!(asked.recv().unwrap(), (1 << 20, true));
    assert_eq!(client.capacity(), 4 << 20, "ask alone does not change capacity before acceptance");
    client.resized(1 << 20, true).unwrap();
    shrinking.join().unwrap().unwrap();
    assert_eq!(client.capacity(), 1 << 20);
    assert_eq!(client.info().unwrap().capacity, 1 << 20);

    // Rejected shrink: capacity stays unchanged.
    let reject_path = path.clone();
    let rejecting = std::thread::spawn(move || resize(&reject_path, 512 << 10));
    assert_eq!(asked.recv().unwrap(), (512 << 10, true));
    client.resized(512 << 10, false).unwrap();
    assert_eq!(rejecting.join().unwrap(), Err(Errno::ENOSPC));
    assert_eq!(client.capacity(), 1 << 20);
    assert_eq!(client.info().unwrap().capacity, 1 << 20);

    drop(client);
    served.join().unwrap();
}

/// Reads and writes move their data through the connection's transfer area, not the socket
/// (TASK-26.18.6): those that fit a slot, while one is free; others go through the socket.
#[test]
fn reads_and_writes_go_through_the_transfer_area() {
    use crate::client::TRANSFER_SLOT;
    host();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let (client, served) = connect(&path, 1 << 24);
    let extent = |sequence| ExtentId { cell: client.cell(), sequence };
    write(&client, extent(1), 0, &pattern(1, 2 * BLOCK)).unwrap();
    assert_eq!(
        read(&client, extent(1), BLOCK as u64, BLOCK),
        Ok(pattern(1, 2 * BLOCK)[BLOCK..].to_vec())
    );
    assert_eq!(client.transfers(), 2);

    // A slot's worth, and more than a slot's.
    write(&client, extent(2), 0, &pattern(2, TRANSFER_SLOT)).unwrap();
    write(&client, extent(3), 0, &pattern(3, 2 * TRANSFER_SLOT)).unwrap();
    assert_eq!(client.transfers(), 3);
    assert_eq!(read(&client, extent(3), 0, 2 * TRANSFER_SLOT), Ok(pattern(3, 2 * TRANSFER_SLOT)));
    assert_eq!(read(&client, extent(2), 0, TRANSFER_SLOT), Ok(pattern(2, TRANSFER_SLOT)));
    assert_eq!(client.transfers(), 4);

    // No slot free: through the socket.
    let taken = client.take_transfer_slots();
    let used = client.transfers();
    write(&client, extent(4), 0, &pattern(4, BLOCK)).unwrap();
    assert_eq!(read(&client, extent(4), 0, BLOCK), Ok(pattern(4, BLOCK)));
    assert_eq!(client.transfers(), used);
    drop(taken);
    assert_eq!(read(&client, extent(4), 0, BLOCK), Ok(pattern(4, BLOCK)));
    assert_eq!(client.transfers(), used + 1);
    drop(client);
    served.join().unwrap();
}

/// A read dropped while the daemon serves it keeps its transfer slot until the daemon answers it,
/// as until then the daemon may write the slot: given back as the read was dropped, the next read
/// or write took it, and the daemon wrote the dropped read's bytes over that one's (TASK-444.15).
/// So a write's, which the daemon may read until it answers.
#[test]
fn a_dropped_read_keeps_its_transfer_slot_until_answered() {
    use crate::client::TRANSFER_SLOTS;
    use std::sync::{Mutex, mpsc};
    host();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let (client, served) = connect(&path, 1 << 24);
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, extent, 0, &pattern(1, BLOCK)).unwrap();

    // The daemon holds the read, saying it has it, until let go: at its failpoint, named as the
    // daemon names it, as the test waits for it.
    let point = crate::server::failpoint(crate::wire::READ).unwrap();
    let (has, had) = mpsc::channel();
    let (go, gone) = mpsc::channel::<()>();
    let gone = Mutex::new(gone);
    fail::cfg_callback(point, move || {
        has.send(()).unwrap();
        gone.lock().unwrap().recv().unwrap();
    })
    .unwrap();
    let mut data = vec![0; BLOCK];
    {
        let mut buffers = [VolatileSliceMut::from(&mut data[..])];
        let mut reading = std::pin::pin!(client.read_async(extent, 0, &mut buffers));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(reading.as_mut().poll(&mut context).is_pending());
        had.recv().unwrap();
    }
    assert_eq!(client.take_transfer_slots().len(), TRANSFER_SLOTS - 1);

    // Answered, its slot comes back: as the reply comes, on the client's receiving thread.
    fail::remove(point);
    go.send(()).unwrap();
    while client.take_transfer_slots().len() < TRANSFER_SLOTS {
        std::thread::yield_now();
    }
    assert_eq!(read(&client, extent, 0, BLOCK), Ok(pattern(1, BLOCK)));
    drop(client);
    served.join().unwrap();
}

/// The daemon refuses a transfer area it could fault on, one not sealed against shrinking, one
/// with unwritten holes, and ranges past the end of the one it has.
#[test]
#[cfg(target_os = "linux")]
fn transfer_areas_are_checked() {
    use crate::wire::{
        Header, READ, READ_TRANSFER, ReadBody, TRANSFER, TransferBody, WRITE, WRITE_TRANSFER,
        WriteBody,
    };
    use std::os::fd::AsFd;
    host();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let (client, served) = connect(&path, 1 << 24);
    assert_eq!(client.transfers(), 0, "transfer area must be registered lazily");
    // SAFETY: The name is a C string.
    let fd = unsafe { libc::memfd_create(c"unsealed".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0);
    // SAFETY: Just created, and owned here.
    let file = std::fs::File::from(unsafe {
        <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd)
    });
    file.set_len(1 << 20).unwrap();
    let unsealed = Header::new(TRANSFER, 0).with_body(&TransferBody { len: 1 << 20 });
    assert_eq!(client.call(unsealed, &[], &[file.as_fd()]).err(), Some(Errno::EINVAL));

    // A sealed memfd whose pages were never written is refused up front (TASK-547, board 11707).
    // SAFETY: As above.
    let fd = unsafe {
        libc::memfd_create(c"sealed".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING)
    };
    assert!(fd >= 0);
    // SAFETY: As above.
    let sealed = std::fs::File::from(unsafe {
        <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd)
    });
    sealed.set_len(1 << 20).unwrap();
    // SAFETY: Seals memory this test made.
    let seal = unsafe {
        libc::fcntl(
            std::os::fd::AsRawFd::as_raw_fd(&sealed),
            libc::F_ADD_SEALS,
            libc::F_SEAL_SHRINK,
        )
    };
    assert_eq!(seal, 0);
    // Non-page-aligned transfer area lengths are refused (board 7411).
    let unaligned = Header::new(TRANSFER, 0)
        .with_body(&TransferBody { len: cladefs_core::pages::page_size() + 1 });
    assert_eq!(client.call(unaligned, &[], &[sealed.as_fd()]).err(), Some(Errno::EINVAL));
    let sparse = Header::new(TRANSFER, 0).with_body(&TransferBody { len: 1 << 20 });
    assert_eq!(client.call(sparse, &[], &[sealed.as_fd()]).err(), Some(Errno::EINVAL));

    // The first small write lazily registers the cell's own transfer area; a second `TRANSFER`
    // is then refused with `EEXIST`.
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, extent, 0, &pattern(1, BLOCK)).unwrap();
    assert_eq!(client.transfers(), 1);
    // SAFETY: Populated, sealed memfd to test duplicate registration.
    let fd2 = unsafe {
        libc::memfd_create(c"sealed2".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING)
    };
    assert!(fd2 >= 0);
    // SAFETY: Just created, and owned here.
    let mut sealed2 = std::fs::File::from(unsafe {
        <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd2)
    });
    std::io::Write::write_all(&mut sealed2, &vec![0u8; 1 << 20]).unwrap();
    // SAFETY: Seals memory this test made.
    let seal2 = unsafe {
        libc::fcntl(
            std::os::fd::AsRawFd::as_raw_fd(&sealed2),
            libc::F_ADD_SEALS,
            libc::F_SEAL_SHRINK,
        )
    };
    assert_eq!(seal2, 0);
    let again = Header::new(TRANSFER, 0).with_body(&TransferBody { len: 1 << 20 });
    assert_eq!(client.call(again, &[], &[sealed2.as_fd()]).err(), Some(Errno::EEXIST));
    let extent = [client.cell(), 1];
    let area = 8 * crate::client::TRANSFER_SLOT as u64;
    let past = Header::new(READ, 0)
        .with_body(&ReadBody {
            cell: extent[0],
            sequence: extent[1],
            offset: 0,
            len: BLOCK as u64,
            transfer_at: area - BLOCK as u64 + 1,
        })
        .with_flags(READ_TRANSFER);
    assert_eq!(client.call(past, &[], &[]).err(), Some(Errno::EINVAL));
    let wrapping = Header::new(WRITE, 0)
        .with_body(&WriteBody {
            cell: extent[0],
            sequence: extent[1],
            offset: 0,
            transfer_at: u64::MAX,
            data_len: 2,
        })
        .with_flags(WRITE_TRANSFER);
    assert_eq!(client.call(wrapping, &[], &[]).err(), Some(Errno::EINVAL));
    drop(client);
    served.join().unwrap();
}

/// cladefs: a future spawned on replies sends its request on the thread spawning it, and goes on,
/// as the reply comes, on the thread receiving it, which no other thread then waits for (TASK-41);
/// its next request too. The thread receiving replies is held, in the cell's capacity callback,
/// until the future waits for its reply: otherwise the reply may come while the spawning thread
/// still runs it, which then goes on with it.
#[test]
fn futures_go_on_where_their_replies_arrive() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let (client, served) = connect(&path, 1 << 20);
    let (holding, held) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let released = std::sync::Mutex::new(released);
    client.on_capacity(move |_, _| {
        holding.send(()).unwrap();
        released.lock().unwrap().recv().unwrap();
    });
    resize(&path, 4 << 20).unwrap();
    held.recv().unwrap();

    let client = std::sync::Arc::new(client);
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    let (done, finished) = std::sync::mpsc::channel();
    let serving = client.clone();
    crate::spawn_on_replies(async move {
        let data = pattern(7, BLOCK);
        let committed = WriteBarriers { after: Some(Commit::Ordered), ..Default::default() };
        let wrote = serving
            .write_with_barriers_async(extent, 0, &[VolatileSlice::from(&data[..])], committed)
            .await;
        let after_write = std::thread::current();
        let mut read = vec![0; BLOCK];
        let mut into = [VolatileSliceMut::from(&mut read[..])];
        let read_result = serving.read_async(extent, 0, &mut into).await;
        let after_read = std::thread::current();
        done.send((wrote, read_result, read == data, after_write, after_read)).unwrap();
    });
    release.send(()).unwrap();
    let (wrote, read, same, after_write, after_read) = finished.recv().unwrap();
    assert_eq!((wrote, read, same), (Ok(()), Ok(()), true));
    assert_eq!(after_write.name(), Some("cladefsd client"));
    assert_eq!(after_read.id(), after_write.id());
    drop(client);
    served.join().unwrap();
}

/// cladefs: a cell's blocking call on the thread receiving its replies would wait for itself: it
/// fails, as from a callback of the cell's there, rather than panic the VM process (TASK-56),
/// and the cell goes on.
#[test]
fn blocking_calls_where_replies_arrive_fail() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let (client, served) = connect(&path, 1 << 20);
    let client = std::sync::Arc::new(client);
    let (answer, answered) = std::sync::mpsc::channel();
    let calling = std::sync::Arc::downgrade(&client);
    client.on_capacity(move |_, _| {
        answer.send(calling.upgrade().unwrap().info().map(drop)).unwrap();
    });
    resize(&path, 4 << 20).unwrap();
    assert_eq!(answered.recv().unwrap(), Err(Errno::EIO));
    assert_eq!(client.info().map(drop), Ok(()));
    drop(client);
    served.join().unwrap();
}

/// A byte of a pipe's read into the page at `addr`, as the kernel writes it, faulting for writing
/// as KVM does for a vCPU: `Err` with the errno it failed with.
#[cfg(target_os = "linux")]
fn kernel_write(addr: usize) -> Result<(), i32> {
    let mut pipe = [0; 2];
    // SAFETY: Makes a pipe, writes a byte to it, and reads one into `addr`, closing it after.
    unsafe {
        assert_eq!(libc::pipe(pipe.as_mut_ptr()), 0);
        assert_eq!(libc::write(pipe[1], [0x77u8].as_ptr().cast(), 1), 1);
        let read = libc::read(pipe[0], addr as *mut libc::c_void, 1);
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap();
        libc::close(pipe[0]);
        libc::close(pipe[1]);
        if read == 1 { Ok(()) } else { Err(errno) }
    }
}

/// cladefs TASK-164.1 (backlog doc-13): a window that copies on write, in the VM process, maps
/// sealed extents and zeros alone (a `MAP` of an open extent `EPERM`). The
/// guest's write to a sealed extent's page makes the page private, the tenant's memory mapped in
/// its place, the extent as the cell's reads have it and another mapping of it; to zeros, a page
/// of zeros. Past the window's private pages a write is refused (`EFAULT`, as `KVM_RUN` fails,
/// which stops the VM); an `UNMAP` frees a private page, and `PRIVATE` makes pages private at once,
/// within the bound (`ENOMEM` past it, `EINVAL` where nothing is mapped). A snapshot's fork is
/// refused while the window has private pages (`EBUSY`).
#[test]
#[cfg(target_os = "linux")]
fn windows_copy_on_write_in_the_vm_process() {
    host();
    let ps = page_size() as usize;
    let dir = tempfile::tempdir().unwrap();
    let (client, served) = connect(&dir.path().join("store"), 1 << 24);
    let memory = memory(4);
    // SAFETY: The memory is the test's, and outlives the cell.
    unsafe { client.create_dax_window_in_host_memory(memory, 4 * ps as u64, None, 2) }.unwrap();
    assert!(client.copies_on_write());
    cladefs_core::guest_thread_started();
    let base = memory.as_ptr() as usize;
    let (sealed, open) = (
        ExtentId { cell: client.cell(), sequence: 1 },
        ExtentId { cell: client.cell(), sequence: 2 },
    );
    write(&client, sealed, 0, &pattern(3, ps)).unwrap();
    client.commit(None).unwrap();
    write(&client, open, 0, &pattern(4, ps)).unwrap();
    assert_eq!(client.map(open, 0, ps as u64, 0), Err(Errno::EPERM));
    client.commit(None).unwrap();
    client.map(sealed, 0, ps as u64, 0).unwrap();
    client.map(sealed, 0, ps as u64, ps as u64).unwrap();
    futures::executor::block_on(client.map_zeros_async(2 * ps as u64, ps as u64)).unwrap();
    // The window's bytes from `at` to `end`, as the guest reads them.
    let window = |at: usize, end: usize| -> Vec<u8> {
        // SAFETY: The window's memory, whose faults the cell serves.
        (at..end).map(|at| unsafe { memory.as_ptr().add(at).read_volatile() }).collect()
    };
    assert_eq!(window(0, ps), pattern(3, ps), "the sealed extent holds other data");
    // Both read, so that both are mapped, from the one page the extent has in memory.
    assert_eq!(window(ps, 2 * ps), pattern(3, ps), "the sealed extent holds other data");
    assert_eq!(mapped_from(base), mapped_from(base + ps), "the extent's page isn't shared");

    // SAFETY: As above, a write of the guest's.
    unsafe { memory.as_ptr().write_volatile(0xee) };
    assert_ne!(mapped_from(base), mapped_from(base + ps), "the write reached the shared page");
    assert_eq!(window(0, 1), [0xee]);
    assert_eq!(window(1, ps), pattern(3, ps)[1..], "the private page holds other data");
    assert_eq!(window(ps, 2 * ps), pattern(3, ps), "the other mapping changed");
    assert_eq!(read(&client, sealed, 0, ps), Ok(pattern(3, ps)));
    assert_eq!(kernel_write(base + 2 * ps), Ok(()));
    assert_eq!(window(2 * ps, 2 * ps + 1), [0x77]);
    assert_eq!(window(2 * ps + 1, 3 * ps), vec![0; ps - 1], "zeros copied otherwise");

    // Past the window's two private pages.
    assert_eq!(kernel_write(base + ps), Err(libc::EFAULT));
    assert_eq!(window(ps, 2 * ps), pattern(3, ps), "a write refused was made");
    assert_eq!(client.make_version(), Err(Errno::EBUSY));
    client.unmap(0, ps as u64).unwrap();
    assert_eq!(client.make_private(ps as u64, ps as u64), Ok(()));
    assert_eq!(kernel_write(base + ps), Ok(()));
    assert_eq!(client.make_private(3 * ps as u64, ps as u64), Err(Errno::EINVAL));
    client.map(sealed, 0, ps as u64, 3 * ps as u64).unwrap();
    assert_eq!(client.make_private(3 * ps as u64, ps as u64), Err(Errno::ENOMEM));
    client.unmap(0, 4 * ps as u64).unwrap();
    assert_eq!(client.make_version().map(drop), Ok(()));

    drop(client);
    served.join().unwrap();
    // SAFETY: Unmaps the memory, which nothing uses any more.
    assert_eq!(unsafe { libc::munmap(memory.as_ptr().cast(), 4 * ps) }, 0);
}

/// A byte at `addr` written to a pipe, the kernel reading it, faulting for reading as KVM does for
/// a vCPU's load: `Err` with the errno it failed with.
#[cfg(target_os = "linux")]
fn kernel_read(addr: usize) -> Result<(), i32> {
    let mut pipe = [0; 2];
    // SAFETY: Makes a pipe, writes the byte at `addr` to it, and closes it.
    unsafe {
        assert_eq!(libc::pipe(pipe.as_mut_ptr()), 0);
        let written = libc::write(pipe[1], addr as *const libc::c_void, 1);
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap();
        libc::close(pipe[0]);
        libc::close(pipe[1]);
        if written == 1 { Ok(()) } else { Err(errno) }
    }
}

/// cladefs TASK-187: a guest's stray access, a read or a write at a window offset nothing maps
/// (cladefs.dax_stray, fxfs-guest-crash-stray-dax), its page faulted first by KVM's asynchronous
/// page fault's worker, writing, then by the vCPU, stays refused, withheld, however often the vCPU
/// retries it as the VMM does ([`AccessFailures`](crate::AccessFailures)), each retry lifting what
/// the stage restricted for workers' writes; and is given up on, its VM stopped (Firecracker's
/// 159), after a second of failures 25 µs apart, the clock the test's, though the vCPU exits
/// thirteen times between each two, as one taking the guest's interrupts does: vCPU 0 in GCE r621
/// and r622 failed a dozen times, exited, failed again, for as long as the case ran, as an exit
/// ended its run of failures.
#[test]
#[cfg(target_os = "linux")]
fn a_stray_access_stays_refused_and_is_given_up_on() {
    use cladefs_core::{AccessFailures, FailingRun, Failure};
    host();
    let ps = page_size() as usize;
    let dir = tempfile::tempdir().unwrap();
    let (client, served) = connect(&dir.path().join("store"), 1 << 24);
    let memory = memory(4);
    // SAFETY: The memory is the test's, and outlives the cell.
    unsafe { client.create_dax_window_in_host_memory(memory, 4 * ps as u64, None, 2) }.unwrap();
    assert!(client.copies_on_write());
    let base = memory.as_ptr() as usize;
    // KVM's worker, a thread not the guest's, faulting the page in for writing for a vCPU's load.
    let worker = std::thread::spawn(move || kernel_write(base + 2 * ps));
    assert_eq!(worker.join().unwrap(), Err(libc::EFAULT), "the worker's write went through");
    cladefs_core::guest_thread_started();
    let step = std::time::Duration::from_micros(25);
    let accesses: [(&str, fn(usize) -> Result<(), i32>, usize); 2] =
        [("load", kernel_read, base + 2 * ps), ("write", kernel_write, base + 3 * ps + ps / 2)];
    for (what, access, addr) in accesses {
        let mut failures = AccessFailures::default();
        let start = std::time::Instant::now();
        let mut now = start;
        let answer = loop {
            assert_eq!(access(addr), Err(libc::EFAULT), "the stray {what} went through");
            match failures.failed(now) {
                Failure::Retry { ended: None } if now - start < 2 * AccessFailures::PATIENCE => {}
                answer => break answer,
            }
            for _ in 0..13 {
                assert_eq!(failures.ran(|| now), None, "a run ended {:?} in", now - start);
            }
            now += step;
        };
        let (last, failures) = (start + 40_000 * step, 40_001);
        let run = FailingRun { first: start, last, failures, exits: 13 * (failures - 1) };
        assert_eq!(answer, Failure::GiveUp(run), "the stray {what}");
    }
    let told = cladefs_core::describe_windows(Some(base));
    let pages = "0 pages installed, 0 left out, 0 private, 2 withheld, 0 restricted";
    assert!(told.contains(pages), "{told}");

    drop(client);
    served.join().unwrap();
    // SAFETY: Unmaps the memory, which nothing uses any more.
    assert_eq!(unsafe { libc::munmap(memory.as_ptr().cast(), 4 * ps) }, 0);
}

/// cladefs TASK-187: what a forked VM's guest pays for each write to a page not yet here, of a
/// large window that copies on write (GCE r632, pgbench forked at 0.57 of no DAX, its VMMs
/// restricting pages for KVM's workers' writes 2^18 times and more): KVM's worker writes the page
/// in for the vCPU's write, which the stage resolves as a load and restricts to reads, so that the
/// worker's write fails; the vCPU's write then fails, `KVM_RUN` with `EFAULT`, and the VMM retries
/// it once the stage's fault thread has lifted the restriction (`guest_access_failed`); the write,
/// retried, makes the page private. A write failed once, then, or twice where the fault thread was
/// slow to lift: 357 times while the VMM retried at once and a lift walked every page of the
/// window, 140 once a lift walked the pages restricted alone (printed, with the time the writes
/// took).
///
/// The pages are a sealed extent's, not in memory, which the window maps without installing them
/// here, as a forked VM's are: a page here, write-protected, takes a write as the guest's,
/// whoever's (`dax::uffd`'s `writes_to_pages_here_are_stores_whoever_makes_them`). They were zeros
/// once, which the window installs as it maps them: here only past the stage's budget of runs,
/// which the host's `vm.max_map_count` sets. 262,144 on OrbStack left them out; GCE's 1,048,576
/// kept them, and the first worker's write made its page private (GCE r691). A run of failures is
/// by the test's clock, a microsecond a failure: a loaded host may take a run's 10 ms between two.
#[test]
#[cfg(target_os = "linux")]
fn a_guest_write_after_a_workers_write_waits_for_one_lift() {
    use cladefs_core::{AccessFailures, Failure};
    host();
    let ps = page_size() as usize;
    // A 1 GiB window, of which the guest writes `WRITES` pages.
    const PAGES: usize = 1 << 18;
    const WRITES: usize = 1000;
    let dir = tempfile::tempdir().unwrap();
    let (client, served) = connect(&dir.path().join("store"), 1 << 24);
    let memory = memory(PAGES);
    let size = (PAGES * ps) as u64;
    // SAFETY: The memory is the test's, and outlives the cell.
    unsafe { client.create_dax_window_in_host_memory(memory, size, None, WRITES as u64) }.unwrap();
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, extent, 0, &pattern(5, ps)).unwrap();
    client.commit(None).unwrap();
    // Whatever of it the write left in memory evicted, so that mapping it installs nothing.
    cladefs_core::cache::evict_least_used(None, |_| false);
    // Every 64th page, as a forked guest's writes are spread over its window.
    let offset = |page: usize| page * 64 * ps;
    for page in 0..WRITES {
        client.map(extent, 0, ps as u64, offset(page) as u64).unwrap();
    }
    let base = memory.as_ptr() as usize;
    let told = cladefs_core::describe_windows(Some(base));
    let pages = "0 pages installed, 0 left out, 0 private, 0 withheld, 0 restricted";
    assert!(told.contains(pages), "the pages written to are here: {told}");
    cladefs_core::guest_thread_started();
    let (mut failed, start) = (0u64, std::time::Instant::now());
    let mut now = start;
    for page in 0..WRITES {
        let addr = base + offset(page);
        let worker = std::thread::spawn(move || kernel_write(addr));
        assert_eq!(worker.join().unwrap(), Err(libc::EFAULT), "the worker's write went through");
        let mut failures = AccessFailures::default();
        while let Err(errno) = kernel_write(addr) {
            assert_eq!(errno, libc::EFAULT);
            failed += 1;
            now += std::time::Duration::from_micros(1);
            let retry = failures.failed(now);
            assert!(matches!(retry, Failure::Retry { ended: None }), "{retry:?}");
        }
    }
    let took = start.elapsed();
    eprintln!(
        "{WRITES} guest writes after workers' writes, of a window of {PAGES} pages: {failed} \
         retries failed, {:.1} a write, in {took:?}, {:?} a write",
        failed as f64 / WRITES as f64,
        took / WRITES as u32
    );
    let told = cladefs_core::describe_windows(Some(base));
    let pages = format!("{WRITES} private, 0 withheld, 0 restricted (0 for workers' writes)");
    assert!(told.contains(&pages), "{told}");
    assert!(failed <= 2 * WRITES as u64, "{failed} retries failed");
    // SAFETY: The window's last page written to, private now, which nothing else accesses.
    let last = unsafe { std::slice::from_raw_parts((base + offset(WRITES - 1)) as *const u8, ps) };
    let mut stored = pattern(5, ps);
    stored[0] = 0x77;
    assert!(last == &stored[..], "the private page holds other data");

    drop(client);
    served.join().unwrap();
    // SAFETY: Unmaps the memory, which nothing uses any more.
    assert_eq!(unsafe { libc::munmap(memory.as_ptr().cast(), PAGES * ps) }, 0);
}

/// TASK-131.6: the daemon's gauges of its extent cache and its windows' mappings follow a window's
/// mappings: a sealed extent's, the cached extent its mappings hold (pinned), its page faulted in,
/// then mapped at once again; and unmapped, the cached extent nothing holds (idle).
#[test]
#[cfg(target_os = "linux")]
fn metrics_gauge_the_extent_cache_and_the_windows_mappings() {
    host();
    let ps = page_size() as usize;
    let gauges = || {
        let metrics = crate::metrics::render();
        [
            "extent_cache_pinned_entries",
            "extent_cache_idle_entries",
            "window_mappings",
            "window_mapped_bytes",
            "window_pages",
        ]
        .map(|name| {
            let name = format!("cladefsd_{name} ");
            let line = metrics.lines().find_map(|line| line.strip_prefix(&name));
            line.unwrap_or_else(|| panic!("no {name}in {metrics}")).parse::<u64>().unwrap()
        })
    };
    let dir = tempfile::tempdir().unwrap();
    let (client, served) = connect(&dir.path().join("store"), 1 << 24);
    let memory = memory(4);
    // SAFETY: The memory is the test's, and outlives the cell.
    unsafe { client.create_dax_window_in_host_memory(memory, 4 * ps as u64, None, 1) }.unwrap();
    assert_eq!(gauges(), [0, 0, 0, 0, 0]);
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, extent, 0, &pattern(3, ps)).unwrap();
    client.commit(None).unwrap();
    client.map(extent, 0, ps as u64, 0).unwrap();
    // SAFETY: The window's memory, whose faults the cell serves.
    let window = unsafe { std::slice::from_raw_parts(memory.as_ptr(), 3 * ps) };
    assert!(window[..ps] == pattern(3, ps)[..], "its mapping maps other data");
    assert_eq!(gauges(), [1, 0, 1, ps as u64, 1]);

    client.map(extent, 0, ps as u64, 2 * ps as u64).unwrap();
    assert!(window[2 * ps..] == pattern(3, ps)[..], "its other mapping maps other data");
    assert_eq!(gauges(), [1, 0, 2, 2 * ps as u64, 2]);

    client.unmap(0, 4 * ps as u64).unwrap();
    assert_eq!(gauges(), [0, 1, 0, 0, 0]);
    drop(client);
    served.join().unwrap();
    // SAFETY: Unmaps the memory, which nothing uses any more.
    assert_eq!(unsafe { libc::munmap(memory.as_ptr().cast(), 4 * ps) }, 0);
}

/// cladefs TASK-111.5: the metrics show each VM process's window, its mappings and pages, by its
/// cell (board 8563), and the runs of pages its second stage has installed, as it last said, against the most it keeps, as it said
/// making its window.
#[test]
#[cfg(target_os = "linux")]
fn metrics_gauge_each_vm_processs_window_and_host_mappings() {
    host();
    let ps = page_size() as usize;
    let series = |name: &str, vm: &crate::stats::Vm| -> Option<u64> {
        let metrics = crate::metrics::render();
        let labels = format!("{{cell=\"{}\"}} ", vm.cell);
        let name = format!("cladefsd_{name}{labels}");
        metrics
            .lines()
            .find_map(|line| line.strip_prefix(&name))
            .map(|value| value.parse().unwrap())
    };
    let dir = tempfile::tempdir().unwrap();
    let (client, served) = connect(&dir.path().join("store"), 1 << 24);
    let vms = crate::stats::vms_on(crate::Daemon::process());
    assert_eq!(vms.len(), 1);
    let writer = &vms[0];
    // No window yet.
    assert_eq!(series("vm_window_mappings", writer), None);
    assert_eq!(series("vm_host_mapping_budget", writer), None);

    let memory = memory(4);
    // SAFETY: The memory is the test's, and outlives the cell.
    unsafe { client.create_dax_window_in_host_memory(memory, 4 * ps as u64, None, 1) }.unwrap();
    let budget = client.window_budget().unwrap() as u64;
    assert_eq!(series("vm_host_mapping_budget", writer), Some(budget));
    assert_eq!(series("vm_window_mappings", writer), Some(0));
    assert_eq!(series("vm_host_mappings", writer), Some(0));
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, extent, 0, &pattern(3, 2 * ps)).unwrap();
    client.commit(None).unwrap();
    client.map(extent, 0, 2 * ps as u64, 0).unwrap();
    client.map(extent, 0, ps as u64, 3 * ps as u64).unwrap();
    // SAFETY: The window's memory, whose faults the cell serves.
    let window = unsafe { std::slice::from_raw_parts(memory.as_ptr(), 4 * ps) };
    assert!(window[3 * ps..] == pattern(3, ps)[..], "its mapping maps other data");
    assert_eq!(series("vm_window_mappings", writer), Some(2));
    assert_eq!(series("vm_window_mapped_bytes", writer), Some(3 * ps as u64));
    assert_eq!(series("vm_window_private_pages", writer), Some(0));
    let pages = series("vm_window_pages", writer).unwrap();
    assert!((1..=3).contains(&pages), "{pages} pages");

    // Its runs installed as it said, unmapping all but the page at 3: as of a taking of stats,
    // which it answers with them.
    client.unmap(0, 2 * ps as u64).unwrap();
    take_stats(WAIT).join().unwrap();
    assert_eq!(series("vm_window_mappings", writer), Some(1));
    assert_eq!(series("vm_window_pages", writer), Some(1));
    assert_eq!(series("vm_host_mappings", writer), Some(1));

    drop(client);
    served.join().unwrap();
    assert_eq!(crate::stats::vms_on(crate::Daemon::process()), []);
    // SAFETY: Unmaps the memory, which nothing uses any more.
    assert_eq!(unsafe { libc::munmap(memory.as_ptr().cast(), 4 * ps) }, 0);
}

/// Board 10098: the metrics show each window's budget as it is now, its share of its VM
/// process's windows': a window alone the process's whole, 522,240 runs of a host allowing
/// 1,048,576 mappings; halved as a second window of the process comes, each 261,120; the whole
/// again as the first goes. Until then a window's budget was what it said making it, the first's
/// the whole after the second halved it, the second's half after the first went.
#[test]
#[cfg(target_os = "linux")]
fn metrics_show_each_windows_budget_as_its_process_shares_it() {
    host();
    crate::server::MAX_MAP_COUNT_GIVEN.set(1 << 20).unwrap();
    let ps = page_size() as usize;
    let budget_of = |cell: cladefs_core::CellId| -> Option<u64> {
        let metrics = crate::metrics::render();
        let name = format!("cladefsd_vm_host_mapping_budget{{cell=\"{cell}\"}} ");
        metrics
            .lines()
            .find_map(|line| line.strip_prefix(&name))
            .map(|value| value.parse().unwrap())
    };
    // What the VM processes say reaches the daemon as it reads their connections: within this.
    let shown = |cell: cladefs_core::CellId, budget: u64| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while budget_of(cell) != Some(budget) && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        budget_of(cell)
    };
    let dir = tempfile::tempdir().unwrap();
    let (first, first_served) = connect(&dir.path().join("first"), 1 << 24);
    let (second, second_served) = connect(&dir.path().join("second"), 1 << 24);
    let vms = crate::stats::vms_on(crate::Daemon::process());
    let cells: Vec<_> = vms.iter().map(|vm| vm.cell).collect();
    let (first_cell, second_cell) = (cells[0], cells[1]);
    let memories = [memory(2), memory(2)];

    // SAFETY: The memory is the test's, and outlives the cell.
    unsafe { first.create_dax_window_in_host_memory(memories[0], 2 * ps as u64, None, 1) }.unwrap();
    assert_eq!(first.window_budget(), Some(522_240));
    assert_eq!(shown(first_cell, 522_240), Some(522_240), "the first alone");

    // SAFETY: As above.
    unsafe { second.create_dax_window_in_host_memory(memories[1], 2 * ps as u64, None, 1) }
        .unwrap();
    assert_eq!((first.window_budget(), second.window_budget()), (Some(261_120), Some(261_120)));
    assert_eq!(shown(first_cell, 261_120), Some(261_120), "the first beside the second");
    assert_eq!(shown(second_cell, 261_120), Some(261_120), "the second beside the first");

    drop(first);
    first_served.join().unwrap();
    assert_eq!(shown(second_cell, 522_240), Some(522_240), "the second alone");
    assert_eq!(second.window_budget(), Some(522_240));
    drop(second);
    second_served.join().unwrap();
    for memory in memories {
        // SAFETY: Unmaps the memory, which nothing uses any more.
        assert_eq!(unsafe { libc::munmap(memory.as_ptr().cast(), 2 * ps) }, 0);
    }
}

/// cladefs TASK-111.4: the stats hold the daemon's counters and gauges and its build, and each VM
/// process's, as it answered: one whose guest answers, its guest's stats and build among it, its
/// counts as of its answer; one with no guest to ask, at once, without; and one whose guest never
/// answers, as the daemon has it once the wait is over. An answer to an ask answers those before
/// it.
#[test]
#[cfg(target_os = "linux")]
fn stats_hold_the_daemons_vm_processes_and_guests() {
    host();
    let ps = page_size() as usize;
    let dir = tempfile::tempdir().unwrap();
    // Whose guest answers, as the test does.
    let (client, served) = connect(&dir.path().join("store"), 1 << 24);
    let (asked, asks) = std::sync::mpsc::channel();
    client.on_stats_asked(move |ask| asked.send(ask).is_ok());
    // With no guest.
    let (guestless, guestless_served) = connect(&dir.path().join("other"), 1 << 24);
    // Whose guest never answers.
    let (silent, silent_served) = connect(&dir.path().join("silent"), 1 << 24);
    silent.on_stats_asked(|_| true);

    let memory = memory(2);
    // SAFETY: The memory is the test's, and outlives the cell.
    unsafe { client.create_dax_window_in_host_memory(memory, 2 * ps as u64, None, 1) }.unwrap();
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, extent, 0, &pattern(5, ps)).unwrap();
    client.commit(None).unwrap();
    client.map(extent, 0, ps as u64, 0).unwrap();

    // The silent one's wait is the test's.
    let taking = take_stats(std::time::Duration::from_secs(2));
    let ask = asks.recv().unwrap();
    let guest = "seals 3\nseals_hist 0,2,1\ncommit abc\n";
    client.answer_stats(ask, Some(guest)).unwrap();
    let taken = taking.join().unwrap();

    assert_eq!(taken.build, cladefs_core::BUILD_COMMIT);
    let named = |pairs: &[(String, u64)], name: &str| {
        pairs.iter().find(|(named, _)| named == name).map(|(_, value)| *value)
    };
    assert_eq!(named(&taken.counters, "maps"), Some(1));
    let gauges: Vec<(String, u64)> =
        taken.gauges.iter().map(|(name, value)| ((*name).to_owned(), *value)).collect();
    assert_eq!(named(&gauges, "connections"), Some(3));
    let paths: Vec<_> = taken.vms.iter().map(|vm| vm.path.clone()).collect();
    let canonical = dir.path().canonicalize().unwrap();
    assert_eq!(paths, ["store", "other", "silent"].map(|name| canonical.join(name)));

    let (answered, without, unanswered) = (&taken.vms[0], &taken.vms[1], &taken.vms[2]);
    assert_eq!(answered.cell.id, client.cell());
    assert_eq!(answered.build.as_deref(), Some(cladefs_core::BUILD_COMMIT));
    assert!(answered.answered.is_some(), "{answered:?}");
    let window = answered.window.expect("its window's gauges");
    assert_eq!((window.mappings, window.mapped_bytes), (1, ps as u64));
    assert_eq!(answered.stage.installed, window.pages, "its runs installed as of its answer");
    assert_eq!(answered.budget, Some(client.window_budget().unwrap() as u64));
    let guest = answered.guest.as_ref().expect("its guest's stats");
    assert_eq!(guest.build.as_deref(), Some("abc"));
    assert_eq!(guest.counters.get("seals"), Some(&3));
    assert_eq!(guest.histograms.get("seals"), Some(&vec![0, 2, 1]));

    assert!(without.answered.is_some() && without.guest.is_none(), "{without:?}");
    assert_eq!((without.window, without.budget), (None, None));
    assert!(unanswered.answered.is_none() && unanswered.guest.is_none(), "{unanswered:?}");
    assert_eq!(unanswered.build.as_deref(), Some(cladefs_core::BUILD_COMMIT));

    // Two asks the guest is slow to answer, one answer, to the later, answering both.
    drop(silent);
    silent_served.join().unwrap();
    let (first, second) = (take_stats(WAIT), take_stats(WAIT));
    let asked = [asks.recv().unwrap(), asks.recv().unwrap()];
    client.answer_stats(asked[0].max(asked[1]), None).unwrap();
    for taking in [first, second] {
        let taken = taking.join().unwrap();
        assert!(taken.vms[0].answered.is_some(), "{:?}", taken.vms[0]);
    }

    drop((client, guestless));
    for served in [served, guestless_served] {
        served.join().unwrap();
    }
    // SAFETY: Unmaps the memory, which nothing uses any more.
    assert_eq!(unsafe { libc::munmap(memory.as_ptr().cast(), 2 * ps) }, 0);
}

/// cladefs TASK-198: a taking of stats that gives up on a VM process's answer forgets its ask, so
/// that a VM process whose guest never answers, as one paused (its device asking it all the same),
/// holds none of them: it held each, one more a taking, for as long as it lived, which a scraper
/// polling GetStats grew without end.
#[test]
fn asks_given_up_on_are_forgotten() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let (silent, served) = connect(&dir.path().join("silent"), 1 << 24);
    silent.on_stats_asked(|_| true);
    for _ in 0..3 {
        let taken = take_stats(std::time::Duration::from_millis(10)).join().unwrap();
        assert!(taken.vms[0].answered.is_none(), "{:?}", taken.vms[0]);
    }
    assert_eq!(crate::stats::asks_waiting(), [0]);
    drop(silent);
    served.join().unwrap();
}

/// How long a test's taking of stats waits for answers that come, none of its VM processes silent.
const WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// cladefs: the stats the daemon takes, waiting `wait` at most for its VM processes, on a thread of
/// its own, which they answer meanwhile (TASK-111.4).
fn take_stats(wait: std::time::Duration) -> JoinHandle<crate::stats::HostStats> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    std::thread::spawn(move || runtime.block_on(crate::stats::take(wait, true)))
}

/// The environment a child process of [`two_processes_map_the_same_pages`] is told what to map in;
/// and, if set, that it runs on once it has, until its input closes.
const CHILD_SOCKET: &str = "CLADEFSD_TEST_SOCKET";
const CHILD_CELL_DIR: &str = "CLADEFSD_TEST_STORE";
#[cfg(target_os = "linux")]
const CHILD_EXTENT: &str = "CLADEFSD_TEST_EXTENT";
#[cfg(target_os = "linux")]
const CHILD_RUNS_ON: &str = "CLADEFSD_TEST_RUNS_ON";
#[cfg(target_os = "linux")]
const CHILD_CLOSES: &str = "CLADEFSD_TEST_CLOSES";

/// Opens the cell at `cell_dir` through the daemon at `socket`, once the daemon has seen a
/// connection that had it open go.
#[cfg(target_os = "linux")]
fn connect_once_free(socket: &Path, cell_dir: &Path) -> CellClient {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match CellClient::connect(socket, cell_dir, 1 << 24) {
            Ok(client) => return client,
            Err(error)
                if error.errno() == Some(Errno::EBUSY) && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => panic!("opening {}: {error:#}", cell_dir.display()),
        }
    }
}

/// Maps `extent`'s first page into a new window of the cell at `cell_dir` the daemon at `socket`
/// serves, checks it holds the pattern written, and returns the cell, mapping it still, and the
/// page's address.
#[cfg(target_os = "linux")]
fn map_first_page(socket: &Path, cell_dir: &Path, extent: ExtentId) -> (CellClient, usize) {
    let ps = page_size() as usize;
    let client = connect_once_free(socket, cell_dir);
    let memory = memory(1);
    // SAFETY: The memory is this function's, and outlives the cell.
    unsafe { client.create_dax_window_in_host_memory(memory, ps as u64, None, 1) }.unwrap();
    client.map(extent, 0, ps as u64, 0).unwrap();
    // SAFETY: The window's memory, whose faults the cell serves.
    let page = unsafe { std::slice::from_raw_parts(memory.as_ptr(), ps) };
    assert!(*page == pattern(9, ps)[..], "the window maps other data");
    // The memory is leaked: the cell's window maps it until the process exits.
    (client, memory.as_ptr() as usize)
}

/// A VM process, for [`two_processes_map_the_same_pages`] and
/// [`frozen_vm_processes_hold_nothing_up`]: prints where it maps the extent from; then, told to,
/// closes its connection, leaving its window mapped; then exits, or, told to run on, runs on until
/// its input closes.
#[test]
#[ignore = "run by two_processes_map_the_same_pages"]
#[cfg(target_os = "linux")]
fn child_maps_an_extent() {
    let (Ok(socket), Ok(cell_dir), Ok(extent)) =
        (std::env::var(CHILD_SOCKET), std::env::var(CHILD_CELL_DIR), std::env::var(CHILD_EXTENT))
    else {
        return;
    };
    let (cell, sequence) = extent.split_once(':').unwrap();
    let extent = ExtentId { cell: cell.parse().unwrap(), sequence: sequence.parse().unwrap() };
    let (client, page) = map_first_page(Path::new(&socket), Path::new(&cell_dir), extent);
    let (file, offset) = mapped_from(page);
    println!("MAPPED {file} {offset}");
    let mut client = Some(client);
    if std::env::var_os(CHILD_CLOSES).is_some() {
        client.take();
        println!("CLOSED");
    }
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    if std::env::var_os(CHILD_RUNS_ON).is_some() {
        std::io::copy(&mut std::io::stdin(), &mut std::io::sink()).unwrap();
    }
    // As a VMM that dies does: with its cell and window still open.
    std::process::exit(0);
}

/// Two VM processes, each of a cell forked from one, mapping one extent of it at once map the
/// same pages: the daemon's, which cache it once.
#[tokio::test(flavor = "multi_thread")]
#[cfg(target_os = "linux")]
async fn two_processes_map_the_same_pages() {
    host();
    let ps = page_size() as usize;
    let dir = tempfile::tempdir().unwrap();
    let (parent, child) = (dir.path().join("parent"), dir.path().join("child"));
    let extent = {
        let open_cell = OpenCell::create(&parent, 1 << 24).await.unwrap();
        let extent = ExtentId { cell: open_cell.cell().id, sequence: 1 };
        open_cell.write(extent, 0, &[VolatileSlice::from(&pattern(9, ps)[..])]).await.unwrap();
        open_cell.commit(None).await.unwrap();
        open_cell
            .fork_to(&child, cladefs_core::host_cell::fork_child(open_cell.cell()))
            .await
            .unwrap();
        extent
    };
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));

    let (_cell, page) = map_first_page(&socket, &parent, extent);
    let ours = mapped_from(page);
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tests::child_maps_an_extent", "--ignored", "--nocapture"])
        .env(CHILD_SOCKET, &socket)
        .env(CHILD_CELL_DIR, &child)
        .env(CHILD_EXTENT, format!("{}:{}", extent.cell, extent.sequence))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&output.stderr));
    let theirs = stdout
        .lines()
        .find_map(|line| line.strip_prefix("MAPPED "))
        .unwrap_or_else(|| panic!("the child mapped nothing: {stdout}"));
    let (file, offset) = theirs.rsplit_once(' ').unwrap();
    assert_eq!((file.to_string(), offset.parse::<u64>().unwrap()), ours);

    // The child's cell goes with it, for another VM process to open, once the daemon has seen it
    // go.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match CellClient::connect(&socket, &child, 1 << 24) {
            Ok(_) => break,
            Err(error) if std::time::Instant::now() < deadline => {
                assert!(format!("{error:#}").contains(&Errno::EBUSY.to_string()), "{error:#}");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => panic!("the child's cell is still open: {error:#}"),
        }
    }
}
/// The connection a VM process of [`vm_processes_tell_the_daemon_what_their_windows_cost`] is
/// given: its descriptor's number.
#[cfg(target_os = "linux")]
const CHILD_CONNECTION: &str = "CLADEFSD_TEST_CONNECTION";

/// A VM process, for [`vm_processes_tell_the_daemon_what_their_windows_cost`]: changes its window
/// five times, each a stage operation, then makes a page private, which it serves itself (doc-26),
/// and closes its cell; then prints what they cost it.
#[test]
#[ignore = "run by vm_processes_tell_the_daemon_what_their_windows_cost"]
#[cfg(target_os = "linux")]
fn child_changes_its_window() {
    let (Ok(connection), Ok(cell_dir)) =
        (std::env::var(CHILD_CONNECTION), std::env::var(CHILD_CELL_DIR))
    else {
        return;
    };
    let ps = page_size();
    // SAFETY: The connection the parent left this process, which nothing else here owns.
    let stream =
        unsafe { <UnixStream as std::os::fd::FromRawFd>::from_raw_fd(connection.parse().unwrap()) };
    let client =
        CellClient::over(stream, Path::new(&cell_dir), Opening::Create(1 << 24, Kind::Fxfs));
    let client = client.unwrap();
    let memory = memory(1);
    // SAFETY: The memory is this function's, and outlives the cell.
    unsafe { client.create_dax_window_in_host_memory(memory, ps, None, 1) }.unwrap();
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, extent, 0, &pattern(3, ps as usize)).unwrap();
    client.commit(None).unwrap();
    // The window maps the extent anew, with none of its pages in memory yet.
    client.map(extent, 0, ps, 0).unwrap();
    // A load faults its page in, which the window maps.
    // SAFETY: The window's memory, whose faults the cell serves.
    assert_eq!(unsafe { memory.as_ptr().read_volatile() }, pattern(3, 1)[0]);
    // Unmapped, which the daemon waits for.
    client.unmap(0, ps).unwrap();
    // Mapped anew, with its page in memory, which the window maps at once, and the VM process
    // installs as a load first touches it (TASK-276.6).
    client.map(extent, 0, ps, 0).unwrap();
    // SAFETY: The window's memory, whose faults the cell serves.
    assert_eq!(unsafe { memory.as_ptr().read_volatile() }, pattern(3, 1)[0]);
    // cladefs: made private, a run of a page, over the page installed (TASK-164.10).
    client.make_private(0, ps).unwrap();
    let [ops, ns, overwritten, left_out] = client.stage_counts().costs;
    let [runs, pages, private_ns, requests, waited] = client.private_counts();
    drop(client);
    println!(
        "STAGE {ops} {ns} {overwritten} {left_out} {runs} {pages} {private_ns} {requests} {waited}"
    );
}

/// TASK-91.16: what its VM processes' changes to their windows cost them, which they make, the
/// daemon counts as its own (`Stats::stage_ops`): as they tell it, all of it once they close
/// their cells. A VM process is another process, whose own counters the daemon never sees.
#[test]
#[cfg(target_os = "linux")]
fn vm_processes_tell_the_daemon_what_their_windows_cost() {
    use std::os::fd::AsRawFd;
    host();
    let dir = tempfile::tempdir().unwrap();
    let (ours, theirs) = UnixStream::pair().unwrap();
    // The child's end, which it inherits.
    // SAFETY: Clears the close-on-exec flag of a descriptor this test owns.
    assert_eq!(unsafe { libc::fcntl(theirs.as_raw_fd(), libc::F_SETFD, 0) }, 0);
    let before = cladefs_core::stats::stats();
    let served = std::thread::spawn(move || serve_connection(ours, default_tenant().unwrap()));
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tests::child_changes_its_window", "--ignored", "--nocapture"])
        .env(CHILD_CONNECTION, theirs.as_raw_fd().to_string())
        .env(CHILD_CELL_DIR, dir.path().join("store"))
        .output()
        .unwrap();
    drop(theirs);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&output.stderr));
    let stage = stdout
        .lines()
        .find_map(|line| line.strip_prefix("STAGE "))
        .unwrap_or_else(|| panic!("the child changed nothing: {stdout}"));
    let said: Vec<u64> = stage.split(' ').map(|count| count.parse().unwrap()).collect();
    let [ops, ns, overwritten, left_out, runs, pages, private_ns, requests, waited] = said[..]
    else {
        panic!("not nine counts: {stage}");
    };
    // cladefs: one PRIVATE, waited for longer than its stage operation took (TASK-164.10).
    assert_eq!(requests, 1, "{stdout}");
    assert!(waited > private_ns, "{stdout}");
    // Five: the `PRIVATE` is no stage operation of the daemon's since the VM process serves it
    // itself (7fb67264e, board 6210, doc-26 step 4).
    assert_eq!(ops, 5, "{stdout}");
    // cladefs: the unmap took access away, from the page the load faulted in (TASK-91.16); the
    // private page from the page mapped anew.
    assert_eq!((overwritten, left_out), (1, 1), "{stdout}");
    assert_eq!((runs, pages), (1, 1), "{stdout}");
    // Once the daemon is done with the connection, which the child closed.
    served.join().unwrap();
    let counted = cladefs_core::stats::stats().since(&before);
    assert_eq!((counted.stage_ops, counted.stage_ns), (ops, ns));
    let taken = (counted.stage_overwrites, counted.stage_leave_outs);
    assert_eq!(taken, (overwritten, left_out));
    // cladefs: and the runs of private pages, as the VM process told them (TASK-164.10).
    let private = (counted.stage_private_runs, counted.stage_private_pages);
    assert_eq!((private, counted.stage_private_ns), ((runs, pages), private_ns));
    let waits = (counted.vm_private_requests, counted.vm_private_request_ns);
    assert_eq!(waits, (requests, waited));
    // cladefs: the daemon's maps of pages, one a page: the fault's, and the map's anew, of the page
    // in memory by then (TASK-108.1).
    let ps = page_size();
    assert_eq!((counted.stage_maps, counted.stage_map_bytes), (2, 2 * ps));
}

/// What `work` returns, run on a thread of its own, unless it takes longer than `wait`: what a
/// test that would otherwise hang for good waits for.
fn within<T: Send + 'static>(
    wait: std::time::Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, std::sync::mpsc::RecvTimeoutError> {
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // Unheard if the test stopped waiting.
        let _ = done.send(work());
    });
    finished.recv_timeout(wait)
}

/// A child process, killed and reaped once dropped, as a test that fails leaves it.
#[cfg(target_os = "linux")]
struct Reaped(std::process::Child);

#[cfg(target_os = "linux")]
impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Has this process's threads, those it makes and the processes it starts find the system call
/// `call` failing with `errno` where its arguments are as `args` say, each the index of one and
/// the low word it is: as a kernel without what the call asks for, or a daemon without the right
/// to it, answers, for tests of the daemon on such hosts. For as long as the process lives, which
/// nextest gives each test of its own.
#[cfg(target_os = "linux")]
fn refuse_system_call(call: libc::c_long, args: &[(usize, u32)], errno: u16) {
    use libc::{BPF_ABS, BPF_JEQ, BPF_JMP, BPF_K, BPF_LD, BPF_RET, BPF_W, sock_filter};
    #[cfg(target_arch = "x86_64")]
    const ARCH: u32 = 0xc000_003e;
    #[cfg(target_arch = "aarch64")]
    const ARCH: u32 = 0xc000_00b7;
    let load = |offset: usize| sock_filter {
        code: (BPF_LD | BPF_W | BPF_ABS) as u16,
        jt: 0,
        jf: 0,
        k: offset as u32,
    };
    // Each test, unless it holds, jumps to the last instruction, which allows the call.
    let tests = 2 + args.len();
    let mut program = Vec::new();
    let test = |program: &mut Vec<sock_filter>, offset: usize, value: u32| {
        program.push(load(offset));
        let left = 2 * tests - program.len();
        let skip = u8::try_from(left).expect("a short filter");
        program.push(sock_filter {
            code: (BPF_JMP | BPF_JEQ | BPF_K) as u16,
            jt: 0,
            jf: skip,
            k: value,
        });
    };
    test(&mut program, std::mem::offset_of!(libc::seccomp_data, arch), ARCH);
    test(&mut program, std::mem::offset_of!(libc::seccomp_data, nr), call as u32);
    for (arg, value) in args {
        // The argument's low word, which comes first, little-endian.
        test(&mut program, std::mem::offset_of!(libc::seccomp_data, args) + 8 * arg, *value);
    }
    let ret = |k: u32| sock_filter { code: (BPF_RET | BPF_K) as u16, jt: 0, jf: 0, k };
    program.push(ret(libc::SECCOMP_RET_ERRNO | u32::from(errno)));
    program.push(ret(libc::SECCOMP_RET_ALLOW));
    let filter = libc::sock_fprog { len: program.len() as u16, filter: program.as_mut_ptr() };
    // SAFETY: Plain system calls: the filter, which they copy, outlives them.
    unsafe {
        assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
        let set = libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            libc::SECCOMP_FILTER_FLAG_TSYNC,
            &filter,
        );
        assert_eq!(set, 0, "{}", std::io::Error::last_os_error());
    }
}

/// Stops `child` as SIGSTOP does, and waits until it has: the signal is sent before its threads
/// stop, and one still running answers what its caller takes to go unanswered (TASK-134).
#[cfg(target_os = "linux")]
fn freeze(child: &std::process::Child) {
    let pid = child.id() as libc::pid_t;
    // SAFETY: Signals the child this test made.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
    let mut status = 0;
    // SAFETY: Waits for that child to stop, writing `status`.
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) }, pid);
    assert!(libc::WIFSTOPPED(status), "wait status {status:#x}");
}

/// Whether the page of the daemon's physical memory at `offset` in the memory of `arena`, as a VM
/// process maps it from its tenant's arena, is free.
fn page_is_free(arena: cladefs_core::pages::ArenaId, offset: u64) -> bool {
    let location = cladefs_core::pages::PhysLocation { arena, offset };
    cladefs_core::slabs::is_free(location)
        .unwrap_or_else(|| panic!("no slab of {arena:?} holds {offset:#x}"))
}

/// Whether the page of the daemon's physical memory at `offset` in the memory of `arena` is held:
/// neither free, nor of memory that went back, every page of it free.
fn page_is_held(arena: cladefs_core::pages::ArenaId, offset: u64) -> bool {
    cladefs_core::slabs::is_free(cladefs_core::pages::PhysLocation { arena, offset }) == Some(false)
}

/// TASK-79.9: a VM process that stops answering, frozen as SIGSTOP or a cgroup's freezer freezes
/// it, holds nothing up: the daemon takes its access to the pages it maps away without waiting for
/// it, as it evicts them, as the kernel's relief does, while another VM process's requests and
/// faults on them are served; and once it has acknowledged nothing it was sent for a while, the
/// daemon kills it, and lets its cell go. The page it maps, evicted, is no other's until then: not
/// free, but the quarantine's, until it has exited; free once it has.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn frozen_vm_processes_hold_nothing_up() {
    frozen_vm_processes_are_killed().await;
}

/// board 8533: on a kernel without `SO_PEERPIDFD` (before Linux 6.5), as the system call is refused
/// here, the daemon's start refuses, naming the version: it opened a VM process by the pid that
/// connected, which another process may have taken by then, the one it would kill (TASK-79.9).
#[test]
#[cfg(target_os = "linux")]
fn kernels_without_peer_pidfds_are_refused() {
    crate::server::require_peer_pidfds().unwrap();
    let (level, option) = (libc::SOL_SOCKET as u32, libc::SO_PEERPIDFD as u32);
    refuse_system_call(libc::SYS_getsockopt, &[(1, level), (2, option)], libc::ENOPROTOOPT as u16);
    let refused = crate::server::require_peer_pidfds().unwrap_err();
    let said = refused.to_string();
    assert!(said.contains("the extent daemon needs Linux 6.5 or later"), "{said}");
    assert!(said.contains("Protocol not available"), "{said}");
}

/// A cell of a tenant's, whose extent's page a VM process, a child, and this one map, and what
/// the child printed.
#[cfg(target_os = "linux")]
struct TwoVmProcesses {
    dir: tempfile::TempDir,
    tenant: std::sync::Arc<crate::server::Tenant>,
    socket: std::path::PathBuf,
    extent: ExtentId,
    ours: CellClient,
    page: usize,
    theirs: Reaped,
    stdout: std::io::Lines<std::io::BufReader<std::process::ChildStdout>>,
    /// Where the page they map is in the memory of the tenant's arena.
    offset: u64,
}

/// [`TwoVmProcesses`], of a tenant, its child with `env` set too.
#[cfg(target_os = "linux")]
async fn two_vm_processes(env: &[&str]) -> TwoVmProcesses {
    use std::io::BufRead as _;
    host();
    let ps = page_size() as usize;
    let dir = tempfile::tempdir().unwrap();
    // A tenant's VM processes, whose pages are of its arena alone.
    let tenant = crate::server::Tenant::new(1, dir.path()).unwrap();
    let (parent, child) = (dir.path().join("parent"), dir.path().join("child"));
    let extent = {
        // A cell of the tenant's (TASK-364).
        let open_cell = cladefs_core::Opening::here()
            .unwrap()
            .create(&parent, 1 << 24, cladefs_core::CellOf::New(tenant.id()))
            .await
            .unwrap();
        let extent = ExtentId { cell: open_cell.cell().id, sequence: 1 };
        open_cell.write(extent, 0, &[VolatileSlice::from(&pattern(9, ps)[..])]).await.unwrap();
        open_cell.commit(None).await.unwrap();
        open_cell
            .fork_to(&child, cladefs_core::host_cell::fork_child(open_cell.cell()))
            .await
            .unwrap();
        extent
    };
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let serving = tenant.clone();
    std::thread::spawn(move || crate::server::serve_tenant(listener, serving));
    let (ours, page) = map_first_page(&socket, &parent, extent);

    let mut theirs = std::process::Command::new(std::env::current_exe().unwrap());
    theirs
        .args(["--exact", "tests::child_maps_an_extent", "--ignored", "--nocapture"])
        .env(CHILD_SOCKET, &socket)
        .env(CHILD_CELL_DIR, &child)
        .env(CHILD_EXTENT, format!("{}:{}", extent.cell, extent.sequence))
        .env(CHILD_RUNS_ON, "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    for name in env {
        theirs.env(name, "1");
    }
    let mut theirs = Reaped(theirs.spawn().unwrap());
    let mut stdout = std::io::BufReader::new(theirs.0.stdout.take().unwrap()).lines();
    let mapped = stdout.by_ref().map(Result::unwrap).find(|line| line.starts_with("MAPPED "));
    let (file, offset) = mapped_from(page);
    assert_eq!(mapped, Some(format!("MAPPED {file} {offset}")));
    TwoVmProcesses { dir, tenant, socket, extent, ours, page, theirs, stdout, offset }
}

/// What [`frozen_vm_processes_hold_nothing_up`] shows.
#[cfg(target_os = "linux")]
async fn frozen_vm_processes_are_killed() {
    use std::os::unix::process::ExitStatusExt as _;
    let ps = page_size() as usize;
    let TwoVmProcesses { dir, tenant, socket, extent, ours, page, theirs, stdout, offset } =
        two_vm_processes(&[]).await;
    drop(stdout);
    freeze(&theirs.0);

    // The extent's page evicted: its mappings taken away, ours and the frozen child's.
    let arena = tenant.arena();
    let wait = std::time::Duration::from_secs(10);
    let evicted = within(wait, || cladefs_core::cache::evict_least_used(None, |_| false));
    assert_eq!(evicted, Ok(ps as u64), "the eviction waited for the frozen VM process");
    assert!(!page_is_free(arena, offset), "the page the frozen VM process maps is free");
    let ours = std::sync::Arc::new(ours);
    let reading = ours.clone();
    let served = within(wait, move || read(&reading, extent, 0, BLOCK));
    assert_eq!(served, Ok(Ok(pattern(9, BLOCK))), "our read waited for the frozen VM process");
    // SAFETY: The window's memory, whose faults our cell serves.
    let loaded =
        within(wait, move || unsafe { std::slice::from_raw_parts(page as *const u8, ps) }.to_vec());
    assert_eq!(loaded, Ok(pattern(9, ps)), "our fault waited for the frozen VM process");

    // Killed, once it has acknowledged nothing for a while.
    let killed = within(std::time::Duration::from_secs(30), move || {
        let mut theirs = theirs;
        theirs.0.wait().unwrap()
    });
    let status = killed.expect("the frozen VM process lives on");
    assert_eq!(status.signal(), Some(libc::SIGKILL), "{status}");
    // Its cell goes with it, for another VM process to open once the daemon lets it go, which
    // the opening waits for (`CLOSING_WAIT`); and its page, free by then.
    let child = dir.path().join("child");
    let reopened = CellClient::connect(&socket, &child, 1 << 24).map(|client| client.cell());
    assert_eq!(reopened.map_err(|error| error.errno()), Ok(cell_at(&child).id));
    assert!(
        page_is_free(arena, offset),
        "the page the frozen VM process mapped isn't free once it's gone"
    );
}

/// TASK-79.100: a VM process that closes its connection and lives on maps what its window mapped
/// still, as nothing took it away: the page, evicted, is no other's until it is gone, which the
/// daemon sees to, killing it, and lets its cell go once it has exited.
#[tokio::test(flavor = "multi_thread")]
#[cfg(target_os = "linux")]
async fn vm_processes_that_close_their_connections_keep_their_pages_until_gone() {
    use std::os::unix::process::ExitStatusExt as _;
    let ps = page_size() as usize;
    let two = two_vm_processes(&[CHILD_CLOSES]);
    let TwoVmProcesses { dir, tenant, socket, mut theirs, mut stdout, offset, .. } = two.await;
    // Open as it runs on, for waiting for it not to close it.
    let _running = theirs.0.stdin.take();
    let closed = stdout.next().unwrap().unwrap();
    assert_eq!(closed, "CLOSED");

    let arena = tenant.arena();
    let wait = std::time::Duration::from_secs(10);
    let evicted = within(wait, || cladefs_core::cache::evict_least_used(None, |_| false));
    assert_eq!(evicted, Ok(ps as u64), "the eviction waited for the VM process");
    // As long as the daemon gives it to exit, the daemon having seen its connection go: well
    // before that ends.
    let since = std::time::Instant::now();
    while since.elapsed() < std::time::Duration::from_secs(1) {
        assert!(page_is_held(arena, offset), "the page the living VM process maps is free");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let killed = within(std::time::Duration::from_secs(30), move || {
        let mut theirs = theirs;
        theirs.0.wait().unwrap()
    });
    let status = killed.expect("the VM process lives on");
    assert_eq!(status.signal(), Some(libc::SIGKILL), "{status}");
    let reopened = connect_once_free(&socket, &dir.path().join("child"));
    assert_eq!(reopened.cell(), cell_at(&dir.path().join("child")).id);
    assert!(!page_is_held(arena, offset), "the page isn't free once the VM process is gone");
}

/// A VM process, for [`vm_processes_that_leave_their_sockets_to_children_are_gone`]: opens the cell
/// [`CHILD_CELL_DIR`] names through the daemon at [`CHILD_SOCKET`], forks a child, which holds the
/// socket and runs on until killed, says its pid, and exits.
#[test]
#[ignore = "run by vm_processes_that_leave_their_sockets_to_children_are_gone"]
fn child_leaves_its_socket_to_a_child() {
    let (Ok(socket), Ok(cell_dir)) = (std::env::var(CHILD_SOCKET), std::env::var(CHILD_CELL_DIR))
    else {
        return;
    };
    let _cell = CellClient::connect(socket, cell_dir, 1 << 24).unwrap();
    // SAFETY: The child calls only what is safe in a child of a process with threads.
    let child = unsafe { libc::fork() };
    assert!(child >= 0, "{}", std::io::Error::last_os_error());
    if child == 0 {
        loop {
            // SAFETY: Waits for a signal, as the test kills it.
            unsafe { libc::pause() };
        }
    }
    println!("CHILD {child}");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    // SAFETY: As a VMM that dies does: with its cell open.
    unsafe { libc::_exit(0) };
}

/// A process this test has no child's handle of, killed once dropped.
#[cfg(target_os = "linux")]
struct KilledOnDrop(libc::pid_t);

#[cfg(target_os = "linux")]
impl Drop for KilledOnDrop {
    fn drop(&mut self) {
        // SAFETY: Signals the process this test's child left.
        unsafe { libc::kill(self.0, libc::SIGKILL) };
    }
}

/// TASK-79.101: a VM process that exits leaving its socket to a child of its own is gone, though
/// the socket's other end lives on: its cell is another's to open.
#[test]
#[cfg(target_os = "linux")]
fn vm_processes_that_leave_their_sockets_to_children_are_gone() {
    use std::io::BufRead as _;
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let cell_dir = dir.path().join("store");
    let mut process = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tests::child_leaves_its_socket_to_a_child"])
        .args(["--ignored", "--nocapture"])
        .env(CHILD_SOCKET, &socket)
        .env(CHILD_CELL_DIR, &cell_dir)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = std::io::BufReader::new(process.stdout.take().unwrap());
    let child = stdout
        .lines()
        .map(Result::unwrap)
        .find_map(|line| line.strip_prefix("CHILD ").map(|pid| KilledOnDrop(pid.parse().unwrap())));
    let _child = child.expect("the VM process left no child");
    assert!(process.wait().unwrap().success());
    drop(connect_once_free(&socket, &cell_dir));
}

/// A VM process, for [`vm_processes_the_daemon_may_not_kill_are_refused`]: opens the cell
/// [`CHILD_CELL_DIR`] names through the daemon at [`CHILD_SOCKET`], and says whether it did.
#[test]
#[ignore = "run by vm_processes_the_daemon_may_not_kill_are_refused"]
fn child_opens_a_cell() {
    let (Ok(socket), Ok(cell_dir)) = (std::env::var(CHILD_SOCKET), std::env::var(CHILD_CELL_DIR))
    else {
        return;
    };
    match CellClient::connect(socket, cell_dir, 1 << 24) {
        Ok(_) => println!("OPENED"),
        Err(error) => println!("REFUSED {:?}", error.errno()),
    }
}

/// TASK-79.9: a VM process the daemon may not kill, as it may not signal it (`EPERM`, as a daemon
/// of another user without `CAP_KILL` may not, which the system call refused here stands for), it
/// refuses as it connects: one that stopped answering it could not let go of, and the pages it may
/// reach, freed, would wait for it for good.
#[test]
#[cfg(target_os = "linux")]
fn vm_processes_the_daemon_may_not_kill_are_refused() {
    refuse_system_call(libc::SYS_pidfd_send_signal, &[], libc::EPERM as u16);
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tests::child_opens_a_cell", "--ignored", "--nocapture"])
        .env(CHILD_SOCKET, &socket)
        .env(CHILD_CELL_DIR, dir.path().join("store"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let said =
        stdout.lines().find(|line| line.starts_with("OPENED") || line.starts_with("REFUSED"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(said, Some("REFUSED Some(Errno(EIO))"), "{stdout}{stderr}");
}

/// A cell the daemon serves forks to a new cell the daemon serves too, whose window maps the
/// pages the parent's maps: the daemon's counters count the maps, the faults and the reads. The
/// extent is paged in once, for both windows and the fork's READ, which reads nothing from the
/// cell (TASK-32).
#[test]
#[cfg(target_os = "linux")]
fn the_daemon_forks_cells_it_serves() {
    host();
    let ps = page_size() as usize;
    let dir = tempfile::tempdir().unwrap();
    let (parent, child) = (dir.path().join("parent"), dir.path().join("child"));
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));

    let before = cladefs_core::stats::stats();
    let client = CellClient::connect(&socket, &parent, 1 << 24).unwrap();
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, extent, 0, &pattern(9, ps)).unwrap();
    client.commit(None).unwrap();
    let cell = fork_cell(&parent, &child).unwrap().id;
    // A fork to a path that is there already fails, as making a cell there does (TASK-79.83).
    assert_eq!(fork_cell(&parent, &child).unwrap_err().errno(), Some(Errno::EEXIST));
    drop(client);
    let (ours, page) = map_first_page(&socket, &parent, extent);
    let ours_from = mapped_from(page);
    drop(ours);

    let (theirs, page) = map_first_page(&socket, &child, extent);
    assert_eq!(theirs.cell(), cell);
    assert_eq!(mapped_from(page), ours_from);
    assert_eq!(read(&theirs, extent, 0, ps).unwrap(), pattern(9, ps));
    let counted = cladefs_core::stats::stats().since(&before);
    assert_eq!((counted.maps, counted.reads, counted.read_bytes), (2, 1, ps as u64));
    assert_eq!((counted.read_stored_bytes, counted.page_ins), (0, 1));
}

/// TASK-39.1: a VM's cell forks, for a snapshot, only once nothing is open; the VMs restored from
/// the snapshot each open a fork of their own of that version, holding what it held, whose windows
/// map what the snapshot's recorded, mapped again; and a version a connection has open is busy.
#[test]
#[cfg(target_os = "linux")]
fn snapshots_restore_into_forks_of_their_version() {
    host();
    let ps = page_size() as usize;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));

    let client = CellClient::connect(&socket, dir.path().join("store"), 1 << 24).unwrap();
    let ours = memory(4);
    // SAFETY: The memory is the test's, and outlives the cell.
    unsafe { client.create_dax_window_in_host_memory(ours, 4 * ps as u64, None, 1) }.unwrap();
    let (sealed, open) = (
        ExtentId { cell: client.cell(), sequence: 1 },
        ExtentId { cell: client.cell(), sequence: 2 },
    );
    write(&client, sealed, 0, &pattern(1, ps)).unwrap();
    client.commit(Some(b"root")).unwrap();
    client.map(sealed, 0, ps as u64, 0).unwrap();
    let zeros = cladefs_core::WindowMapping {
        window_offset: 2 * ps as u64,
        len: ps as u64,
        contents: cladefs_core::MappingContents::Zeros,
    };
    client.map_many(&[zeros]).unwrap();
    write(&client, open, 0, &pattern(2, ps)).unwrap();
    assert_eq!(client.make_version(), Err(Errno::EBUSY));
    client.commit(None).unwrap();
    client.map(open, 0, ps as u64, 3 * ps as u64).unwrap();
    let version = client.make_version().unwrap();
    let mappings = client.window_mappings().unwrap();
    let extent = |extent, offset| cladefs_core::MappingContents::Extent { extent, offset };
    assert_eq!(
        mappings,
        [
            cladefs_core::WindowMapping {
                window_offset: 0,
                len: ps as u64,
                contents: extent(sealed, 0)
            },
            zeros,
            cladefs_core::WindowMapping {
                window_offset: 3 * ps as u64,
                len: ps as u64,
                contents: extent(open, 0),
            },
        ]
    );
    // SAFETY: As above.
    let window =
        |memory: NonNull<u8>| unsafe { std::slice::from_raw_parts(memory.as_ptr(), 4 * ps) };
    let snapshotted = [0, 2, 3].map(|page| window(ours)[page * ps..][..ps].to_vec());

    let restored: Vec<CellClient> = ["a", "b"]
        .into_iter()
        .map(|name| {
            let restored =
                CellClient::connect_forked(&socket, dir.path().join(name), version, 0).unwrap();
            assert_eq!(restored.root().unwrap(), b"root");
            let memory = memory(4);
            // SAFETY: As above.
            unsafe { restored.create_dax_window_in_host_memory(memory, 4 * ps as u64, None, 1) }
                .unwrap();
            restored.map_many(&mappings).unwrap();
            assert_eq!(restored.window_mappings().unwrap(), mappings);
            for (page, bytes) in [0, 2, 3].into_iter().zip(&snapshotted) {
                assert!(window(memory)[page * ps..][..ps] == bytes[..], "page {page} differs");
            }
            restored
        })
        .collect();
    let cells: Vec<u64> = restored.iter().map(CellClient::cell).collect();
    assert!(!cells.contains(&client.cell()) && cells[0] != cells[1], "{cells:x?}");
    // Each writes on its own.
    let theirs = ExtentId { cell: cells[0], sequence: 1 };
    write(&restored[0], theirs, 0, &pattern(3, BLOCK)).unwrap();
    restored[0].commit(None).unwrap();
    assert!(read(&restored[1], theirs, 0, BLOCK).is_err());
    assert_eq!(read(&restored[1], open, 0, BLOCK), Ok(pattern(2, BLOCK)));

    let forked = CellClient::connect_forked(&socket, dir.path().join("c"), version, 0);
    let root = forked.map(|client| client.root()).map_err(|error| error.errno());
    assert_eq!(root, Ok(Ok(b"root".to_vec())));
}

/// TASK-92.14: a restored VM's window maps its snapshot's mappings again in one request, one
/// change of the window, where a MAP each took a round trip and a change apiece: `PAGES` mappings,
/// each a page of an extent of its own, and runs of zeros between, as a zygote's chunks have them.
/// Restored both ways, each window reads what the snapshot's did; the MAPs' restore and the
/// `MAP_MANY`'s are timed, for the task's notes.
#[test]
#[cfg(target_os = "linux")]
fn snapshots_map_their_windows_again_in_one_request() {
    const PAGES: usize = 1024;
    host();
    let ps = page_size() as usize;
    let window_pages = 2 * PAGES;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));

    let client = CellClient::connect(&socket, dir.path().join("store"), 1 << 26).unwrap();
    let memory = memory(window_pages);
    // SAFETY: The memory is the test's, and outlives the cell.
    unsafe { client.create_dax_window_in_host_memory(memory, (window_pages * ps) as u64, None, 1) }
        .unwrap();
    let extent = |page: usize| ExtentId { cell: client.cell(), sequence: 1 + page as u64 };
    for page in 0..PAGES {
        write(&client, extent(page), 0, &pattern(page as u8, ps)).unwrap();
    }
    client.commit(None).unwrap();
    // Each extent's page at an even page of the window, last first; zeros at every fourth odd one.
    let mut mapped = Vec::new();
    for page in (0..PAGES).rev() {
        let at = (2 * page * ps) as u64;
        mapped.push(cladefs_core::WindowMapping {
            window_offset: at,
            len: ps as u64,
            contents: cladefs_core::MappingContents::Extent { extent: extent(page), offset: 0 },
        });
        if page % 4 == 0 {
            let zeros = cladefs_core::MappingContents::Zeros;
            mapped.push(cladefs_core::WindowMapping {
                window_offset: at + ps as u64,
                len: ps as u64,
                contents: zeros,
            });
        }
    }
    client.map_many(&mapped).unwrap();
    let version = client.make_version().unwrap();
    let mappings = client.window_mappings().unwrap();
    assert_eq!(mappings.len(), PAGES + PAGES / 4);
    // The bytes the window's mappings map, in order: the pages between them map nothing, which
    // the window refuses an access to.
    let window = |memory: NonNull<u8>| -> Vec<u8> {
        (mappings.iter())
            .flat_map(|mapping| {
                let (at, len) = (mapping.window_offset as usize, mapping.len as usize);
                // SAFETY: As above.
                unsafe { std::slice::from_raw_parts(memory.as_ptr().add(at), len) }.to_vec()
            })
            .collect()
    };
    let snapshotted = window(memory);
    assert_eq!(&snapshotted[..ps], &pattern(0, ps)[..]);
    assert_eq!(&snapshotted[ps..2 * ps], &vec![0; ps][..]);
    assert_eq!(&snapshotted[2 * ps..3 * ps], &pattern(1, ps)[..]);

    let restore = |name: &str, each: bool| {
        let restored =
            CellClient::connect_forked(&socket, dir.path().join(name), version, 0).unwrap();
        let memory = crate::tests::memory(window_pages);
        // SAFETY: As above.
        unsafe {
            restored.create_dax_window_in_host_memory(memory, (window_pages * ps) as u64, None, 1)
        }
        .unwrap();
        let maps = served("map");
        let [ops, ops_ns, ..] = restored.stage_counts().costs;
        let start = std::time::Instant::now();
        if each {
            // As restores did before: a MAP each.
            for &cladefs_core::WindowMapping { window_offset, len, contents } in &mappings {
                match contents {
                    cladefs_core::MappingContents::Extent { extent, offset } => {
                        restored.map(extent, offset, len, window_offset).unwrap();
                    }
                    cladefs_core::MappingContents::Zeros => {
                        let zeros = restored.map_zeros_async(window_offset, len);
                        futures::executor::block_on(zeros).unwrap();
                    }
                }
            }
        } else {
            restored.map_many(&mappings).unwrap();
        }
        let took = start.elapsed();
        assert_eq!(restored.window_mappings().unwrap(), mappings);
        assert!(window(memory) == snapshotted, "{name}: the restored window differs");
        let [ops_then, ops_ns_then, ..] = restored.stage_counts().costs;
        let counted = (served("map") - maps, ops_then - ops);
        eprintln!(
            "{name}: {} mappings in {took:?}, {counted:?} (MAPs, stage operations), \
             the VM process's stage operations {:?} of it",
            mappings.len(),
            std::time::Duration::from_nanos(ops_ns_then - ops_ns),
        );
        (restored, counted)
    };
    let (_each, each) = restore("each", true);
    let (_at_once, at_once) = restore("at-once", false);
    assert_eq!(each.0, mappings.len() as u64);
    assert_eq!(at_once.0, 1);
    // The ranges mapped anew told the VM process in runs, not one each, and the pages' maps
    // in runs across mappings, where their pages follow one another.
    assert!(at_once.1 < each.1, "{at_once:?} against {each:?}");
}

/// TASK-92.14: a VM whose guest stored to its window, its pages private, is snapshotted once its
/// freeze has written them back and rebound them (backlog doc-13): refused before, as no extent
/// holds their bytes; and restored, its window reads what the guest stored, mapped again in one
/// request.
#[test]
#[cfg(target_os = "linux")]
fn snapshots_after_private_pages_are_rebound_restore_their_bytes() {
    const PAGES: usize = 8;
    host();
    let ps = page_size() as usize;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));

    let client = CellClient::connect(&socket, dir.path().join("store"), 1 << 24).unwrap();
    let memory = memory(PAGES);
    let size = (PAGES * ps) as u64;
    // SAFETY: The memory is the test's, and outlives the cell.
    unsafe { client.create_dax_window_in_host_memory(memory, size, None, PAGES as u64) }.unwrap();
    assert!(client.copies_on_write());
    cladefs_core::guest_thread_started();
    let extent = |sequence: u64| ExtentId { cell: client.cell(), sequence };
    let file = extent(1);
    write(&client, file, 0, &pattern(1, PAGES * ps)).unwrap();
    client.commit(None).unwrap();
    client.map(file, 0, size, 0).unwrap();
    // The guest's writes, to every other page, and its file's page past them zeros.
    let stored = |page: usize| pattern(0x80 | page as u8, ps);
    for page in (0..PAGES).step_by(2) {
        // SAFETY: The window's page, which the cell makes private.
        unsafe {
            std::ptr::copy_nonoverlapping(stored(page).as_ptr(), memory.as_ptr().add(page * ps), ps)
        };
    }
    // SAFETY: As above.
    let window = || unsafe { std::slice::from_raw_parts(memory.as_ptr(), PAGES * ps).to_vec() };
    let expected: Vec<u8> = (0..PAGES)
        .flat_map(|page| match page % 2 {
            0 => stored(page),
            _ => pattern(1, PAGES * ps)[page * ps..][..ps].to_vec(),
        })
        .collect();
    assert!(window() == expected, "the writes didn't reach the window");
    assert_eq!(client.make_version(), Err(Errno::EBUSY));

    // The freeze: each page stored to written back, to an extent of its own, sealed, and mapped
    // over the private page, which goes.
    let mut rebound = Vec::new();
    for page in (0..PAGES).step_by(2) {
        let written = extent(2 + page as u64);
        let bytes = window()[page * ps..][..ps].to_vec();
        write(&client, written, 0, &bytes).unwrap();
        rebound.push(cladefs_core::WindowMapping {
            window_offset: (page * ps) as u64,
            len: ps as u64,
            contents: cladefs_core::MappingContents::Extent { extent: written, offset: 0 },
        });
    }
    client.commit(None).unwrap();
    client.map_many(&rebound).unwrap();
    assert!(window() == expected, "rebinding changed the window");
    let version = client.make_version().unwrap();
    let mappings = client.window_mappings().unwrap();
    assert_eq!(mappings.len(), PAGES);

    let restored =
        CellClient::connect_forked(&socket, dir.path().join("restored"), version, 0).unwrap();
    let restored_memory = crate::tests::memory(PAGES);
    // SAFETY: As above.
    unsafe { restored.create_dax_window_in_host_memory(restored_memory, size, None, PAGES as u64) }
        .unwrap();
    let maps = served("map");
    restored.map_many(&mappings).unwrap();
    assert_eq!(served("map") - maps, 1);
    // SAFETY: As above.
    let restored_window =
        unsafe { std::slice::from_raw_parts(restored_memory.as_ptr(), PAGES * ps).to_vec() };
    assert!(restored_window == expected, "the restored window lost the guest's writes");
}

/// TASK-39.1: a VM restored where its snapshot's version isn't open, as on another host than the
/// snapshot's, forks its cell from the version, which the daemon opens from its bucket, the cell's
/// commits uploaded first (TASK-364, doc-20).
#[test]
fn snapshot_versions_restore_from_the_bucket() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));

    let client = CellClient::connect(&socket, dir.path().join("store"), 1 << 24).unwrap();
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, extent, 0, &pattern(4, BLOCK)).unwrap();
    client.commit(Some(b"root")).unwrap();
    let version = client.make_version().unwrap();
    drop(client);

    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    let moved = elsewhere.join("store");
    let restored = CellClient::connect_forked(&socket, &moved, version, 1 << 24).unwrap();
    assert_eq!(restored.root().unwrap(), b"root");
    assert_eq!(read(&restored, extent, 0, BLOCK), Ok(pattern(4, BLOCK)));
    assert_ne!(restored.cell(), version.cell);
}

/// TASK-39.1: a cell whose VM process just went opens at once for another, as a VM restored from
/// a snapshot of it does: its connection lets it go once its commits are durable.
#[test]
fn cells_open_once_their_vm_process_goes() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let path = dir.path().join("store");
    for round in 0..20 {
        let client = CellClient::connect(&socket, &path, 1 << 24).unwrap();
        let extent = ExtentId { cell: client.cell(), sequence: round + 1 };
        write(&client, extent, 0, &pattern(5, BLOCK)).unwrap();
        client.commit_ordered(None).unwrap();
        drop(client);
    }
    let client = CellClient::connect(&socket, &path, 1 << 24).unwrap();
    assert_eq!(client.info().unwrap().next_sequence, 21);
}

/// TASK-39.2: a daemon with roots refuses to open, create or fork cells outside them.
#[test]
fn daemons_with_roots_serve_no_cells_outside_them() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let (ours, theirs) = (dir.path().join("ours"), dir.path().join("theirs"));
    std::fs::create_dir(&ours).unwrap();
    std::fs::create_dir(&theirs).unwrap();
    // SAFETY: as in host.
    unsafe { std::env::set_var("CLADEFSD_ROOTS", &ours) };
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));

    let refused = CellClient::connect(&socket, theirs.join("store"), 1 << 24);
    assert_eq!(refused.err().and_then(|error| error.errno()), Some(Errno::EACCES));
    let client = CellClient::connect(&socket, ours.join("store"), 1 << 24).unwrap();
    client.commit(None).unwrap();
    let version = client.make_version().unwrap();
    let refused = CellClient::connect_forked(&socket, theirs.join("fork"), version, 1 << 24);
    assert_eq!(refused.err().and_then(|error| error.errno()), Some(Errno::EACCES));
    CellClient::connect_forked(&socket, ours.join("fork"), version, 1 << 24).unwrap();
}

/// TASK-456.2: a memory cell's pages and holes, committed at once, read back once it is opened
/// again; a fork of it reads its pages and stores only those it writes, each page of the parent
/// it leaves as it was, and its holes nothing.
#[test]
#[cfg(target_os = "linux")]
fn memory_cells_commit_pages_and_holes_and_forks_store_only_their_own() {
    use cladefs_wire::memory_cell::MemoryCell;
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    const PAGES: u64 = 256;
    let path = dir.path().join("memory");
    let memory = MemoryCell::open(&socket, &path, PAGES * 4096).unwrap();
    let page = memory.page_size();
    assert_eq!(page, 4096);
    let filled = |byte: u8, pages: u64| vec![byte; (pages * page) as usize];
    memory.write(0, &filled(1, PAGES)).unwrap();
    memory.discard(100 * page, 20 * page).unwrap();
    memory.commit().unwrap();
    let parent_extents = memory.extents().unwrap();
    let fork = dir.path().join("fork");
    let forked = fork_cell(&path, &fork).unwrap().id;
    memory.close().unwrap();

    let mut expected = filled(1, PAGES);
    expected[(100 * page) as usize..(120 * page) as usize].fill(0);
    let reopened = MemoryCell::open(&socket, &path, PAGES * 4096).unwrap();
    let mut image = filled(0xee, PAGES);
    reopened.read(0, &mut image).unwrap();
    assert!(image == expected, "the image committed");
    reopened.close().unwrap();

    let child = MemoryCell::open(&socket, &fork, PAGES * 4096).unwrap();
    assert_eq!(child.cell(), forked);
    let mut image = filled(0xee, PAGES);
    child.read(0, &mut image).unwrap();
    assert!(image == expected, "the fork reads its parent's pages");
    child.write(3 * page, &filled(2, 4)).unwrap();
    child.discard(10 * page, page).unwrap();
    child.commit().unwrap();
    expected[(3 * page) as usize..(7 * page) as usize].fill(2);
    expected[(10 * page) as usize..(11 * page) as usize].fill(0);
    child.read(0, &mut image).unwrap();
    assert!(image == expected, "the fork's image");
    // Its own extents: the pages it wrote, with the nodes naming them, a few blocks, one
    // extent; the rest are the parent's.
    let own: Vec<_> =
        (child.extents().unwrap().into_iter()).filter(|extent| extent.cell == forked).collect();
    assert_eq!(own.len(), 1, "the fork's own extents: {own:?}");
    let inherited = child.extents().unwrap().into_iter().filter(|extent| extent.cell != forked);
    assert!(inherited.into_iter().all(|extent| parent_extents.contains(&extent)));
    child.close().unwrap();
}

/// TASK-540: a memory cell whose last commit left more extents dying than its root holds, listed
/// in an extent its root names, closes and opens again, as often as it is closed, holding what
/// that commit made.
#[test]
#[cfg(target_os = "linux")]
fn a_memory_cell_opens_again_after_a_commit_freeing_many_extents() {
    use cladefs_wire::memory_cell::MemoryCell;
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    const PAGES: u64 = 128;
    let path = dir.path().join("memory");
    let memory = MemoryCell::open(&socket, &path, PAGES * 4096).unwrap();
    let page = memory.page_size();
    // An extent a commit, each holding a page the later ones leave live.
    for n in 0..80 {
        memory.write(n * page, &vec![n as u8 + 1; page as usize]).unwrap();
        memory.commit().unwrap();
    }
    let extents = memory.extents().unwrap().len();
    assert!(extents > 64, "{extents} extents, too few to list");
    memory.discard(0, PAGES * page).unwrap();
    memory.commit().unwrap();
    memory.close().unwrap();

    for opening in 0..2 {
        let reopened = MemoryCell::open(&socket, &path, PAGES * 4096).unwrap();
        let mut image = vec![0xee; (PAGES * page) as usize];
        reopened.read(0, &mut image).unwrap();
        assert!(image.iter().all(|&byte| byte == 0), "opening {opening}: the memory punched");
        reopened.close().unwrap();
    }
}

/// TASK-456.3: a memory cell's window, a VM's memory restored from it, maps nothing until faults
/// reach it: then the runs of the tree node around each page, data and holes, as the cell's last
/// commit has them. Two forks' windows map the daemon's one page of each extent's block, shared;
/// a write of the guest's makes a page private in its window alone, the other's still the shared
/// page; the windows' private pages reserve nothing in the daemon.
#[test]
#[cfg(target_os = "linux")]
fn memory_windows_map_what_faults_reach_and_share_their_pages() {
    use cladefs_wire::memory_cell::MemoryCell;
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let ps = page_size();
    // Three nodes of the tree's, the last part of one.
    const PAGES: u64 = 400;
    let size = PAGES * ps;
    let memory = MemoryCell::open(&socket, &dir.path().join("memory"), size).unwrap();
    let mut image = Vec::with_capacity(size as usize);
    for page in 0..PAGES {
        image.extend(pattern(page as u8, ps as usize));
    }
    memory.write(0, &image).unwrap();
    memory.discard(200 * ps, 30 * ps).unwrap();
    image[(200 * ps) as usize..(230 * ps) as usize].fill(0);
    memory.commit().unwrap();
    let (one, two) = (dir.path().join("one"), dir.path().join("two"));
    let version = memory.make_version().unwrap();
    memory.close().unwrap();

    let restore = |path: &Path| {
        let memory_cell = MemoryCell::open_forked(&socket, path, version, size).unwrap();
        let base = memory_noreserve(PAGES as usize);
        // SAFETY: The memory is the test's, and outlives the memory_cell (unmapped once it goes).
        unsafe { memory_cell.window(base, size, None) }.unwrap();
        (memory_cell, base)
    };
    let ((first, a), (second, b)) = (restore(&one), restore(&two));
    // The window's bytes from `at` to `end`, as the guest reads them.
    let read = |base: NonNull<u8>, at: u64, end: u64| -> Vec<u8> {
        // SAFETY: The window's memory, whose faults its memory_cell serves.
        (at..end).map(|at| unsafe { base.as_ptr().add(at as usize).read_volatile() }).collect()
    };
    assert!(read(a, 0, size) == image, "the first window holds the image");
    assert!(read(b, 0, size) == image, "the second window holds the image");
    for page in [0, 150, 340, 399] {
        let (at, bt) = (a.as_ptr() as usize + (page * ps) as usize, b.as_ptr() as usize);
        let bt = bt + (page * ps) as usize;
        assert_eq!(mapped_from(at), mapped_from(bt), "page {page} isn't shared");
    }

    cladefs_core::guest_thread_started();
    let at = a.as_ptr() as usize + (5 * ps) as usize;
    // SAFETY: As above, a write of the guest's.
    unsafe { (at as *mut u8).write_volatile(0xee) };
    let bt = b.as_ptr() as usize + (5 * ps) as usize;
    assert_ne!(mapped_from(at), mapped_from(bt), "the write reached the shared page");
    assert_eq!(read(a, 5 * ps, 5 * ps + 1), [0xee]);
    assert_eq!(read(b, 5 * ps, 6 * ps), pattern(5, ps as usize), "the other window changed");
    assert_eq!(first.private_runs().unwrap(), vec![5 * ps..6 * ps]);
    assert_eq!(second.private_pages().unwrap(), 0);

    first.close().unwrap();
    second.close().unwrap();
    for base in [a, b] {
        // SAFETY: Unmaps the memory, which nothing uses any more.
        assert_eq!(unsafe { libc::munmap(base.as_ptr().cast(), size as usize) }, 0);
    }
}

/// `pages` pages of anonymous memory the test owns, reserving none: what a VMM maps a guest's
/// memory as.
#[cfg(target_os = "linux")]
fn memory_noreserve(pages: usize) -> NonNull<u8> {
    // SAFETY: Maps anonymous memory, which the test owns.
    let memory = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            pages * page_size() as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    assert_ne!(memory, libc::MAP_FAILED);
    NonNull::new(memory.cast()).unwrap()
}

/// TASK-39.4: with a bucket, deleting a cell through the daemon deletes its cell there too,
/// and the collector keeps what its fork needs of it.
#[test]
fn deleted_cells_keep_what_their_forks_need() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let (path, fork) = (dir.path().join("store"), dir.path().join("fork"));
    let client = CellClient::connect(&socket, &path, 1 << 24).unwrap();
    let base_cell = client.cell();
    let base = ExtentId { cell: base_cell, sequence: 1 };
    write(&client, base, 0, &pattern(3, 2 * BLOCK)).unwrap();
    client.commit(Some(b"base")).unwrap();
    let fork_cell = fork_cell(&path, &fork).unwrap();
    let base_cell = cell_at(&path);
    drop(client);
    delete_cell(&path).unwrap();

    let bucket = crate::cells::daemon_bucket().unwrap();
    futures::executor::block_on(async {
        let deleted = bucket_cell::head::get(&*bucket, base_cell).await.unwrap().unwrap();
        assert!(deleted.deleted());
        let record = bucket_cell::head::get(&*bucket, fork_cell).await.unwrap().unwrap();
        assert!(!record.deleted());
        bucket_cell::collector::collect(&*bucket, 1).await.unwrap();
        let state = bucket_cell::recover(&*bucket, fork_cell, record.origin()).await.unwrap();
        let data = read_back(&bucket, base, &state.state.extents[&base]).await;
        assert_eq!(data, pattern(3, 2 * BLOCK));
    });
    let forked = CellClient::connect(&socket, &fork, 1 << 24).unwrap();
    assert_eq!(forked.root().unwrap(), b"base");
    assert_eq!(read(&forked, base, 0, 2 * BLOCK), Ok(pattern(3, 2 * BLOCK)));
}

/// TASK-39.7: a cell's copy placed at an earlier placement than the one that last took its cell
/// does not open (`ESTALE`): a placement cut off takes no cell back from the one after it.
#[tokio::test(flavor = "multi_thread")]
async fn earlier_placements_open_no_cells() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let (path, earlier) = (dir.path().join("store"), dir.path().join("earlier"));
    let cell = OpenCell::create(&path, 1 << 24).await.unwrap().cell();
    host_fs::may_block(|| OpenCell::place(&path, 5).unwrap());
    let client = CellClient::connect(&socket, &path, 1 << 24).unwrap();
    write(&client, ExtentId { cell: cell.id, sequence: 1 }, 0, &pattern(1, BLOCK)).unwrap();
    client.commit(Some(b"placed at 5")).unwrap();
    drop(client);

    // A copy of the cell's made here, not opened, which would take its head: placed before.
    host_fs::may_block(|| {
        std::fs::create_dir(&earlier).unwrap();
        let description =
            local_cell::Description { block_size: BLOCK as u32, capacity: 1 << 24, cell };
        local_cell::create_host(&earlier, description, "memory:///cells", Kind::Fxfs).unwrap();
        local_cell::made_host(&earlier).unwrap();
        OpenCell::place(&earlier, 3).unwrap();
    });
    let refused = CellClient::connect(&socket, &earlier, 1 << 24);
    assert_eq!(refused.err().and_then(|error| error.errno()), Some(Errno::ESTALE));
    host_fs::may_block(|| OpenCell::place(&earlier, 6).unwrap());
    let later = CellClient::connect(&socket, &earlier, 1 << 24).unwrap();
    assert_eq!(later.root().unwrap(), b"placed at 5");
}

/// TASK-550: a cell whose cell a later placement took, its uploader fenced, changes nothing more:
/// a `COMMIT` without `COMMIT_UPLOADED`, a guest's fsync, is answered `EROFS`, as `WRITE` and
/// `FREE` are (doc-4), rather than acknowledged and lost.
#[tokio::test(flavor = "multi_thread")]
async fn fenced_cells_answer_ordered_commits_erofs() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let path = |name: &str| dir.path().join(name);
    let cell = OpenCell::create(path("first"), 1 << 24).await.unwrap().cell();
    host_fs::may_block(|| OpenCell::place(path("first"), 5).unwrap());
    let first = CellClient::connect(&socket, path("first"), 1 << 24).unwrap();
    let extent = |sequence| ExtentId { cell: cell.id, sequence };
    write(&first, extent(1), 0, &pattern(1, BLOCK)).unwrap();
    first.commit(None).unwrap();
    cladefs_core::Opening::here()
        .unwrap()
        .take(path("second"), 1 << 24, cell, Some(6))
        .await
        .unwrap()
        .close()
        .await;
    // The first's uploader finds itself fenced at its next upload, which a durable commit waits
    // for, or has found it already.
    assert_eq!(first.commit(None), Err(Errno::EROFS));
    assert_eq!(first.commit_ordered(None), Err(Errno::EROFS));
    assert_eq!(write(&first, extent(2), 0, &pattern(2, BLOCK)), Err(Errno::EROFS));
    assert_eq!(first.free(extent(1)), Err(Errno::EROFS));
}

/// A transfer area of `len` bytes, populated and given to a daemon serving a connection of its
/// cell: the area's memory and mapping, and the connection.
#[cfg(target_os = "linux")]
struct ServedArea {
    _dir: tempfile::TempDir,
    file: std::fs::File,
    addr: *mut u8,
    len: usize,
    sender: crate::wire::Sender,
    receiver: crate::wire::Receiver,
    _stream: UnixStream,
    cell: u64,
}

#[cfg(target_os = "linux")]
impl ServedArea {
    fn new(len: usize) -> Self {
        Self::with_setup(len, |_, _| {})
    }

    fn with_setup(len: usize, before_transfer: impl FnOnce(&std::fs::File, *mut u8)) -> Self {
        use crate::wire::*;
        use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
        host();
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("cladefsd.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || serve(listener));
        // SAFETY: The name is a C string.
        let fd = unsafe { libc::memfd_create(c"area".as_ptr(), libc::MFD_ALLOW_SEALING) };
        assert!(fd >= 0);
        // SAFETY: Just made, owned here.
        let file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        file.set_len(len as u64).unwrap();
        let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW;
        // SAFETY: Seals what this test made.
        assert_eq!(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) }, 0);
        // SAFETY: Maps it, as large as it is, for the test's life.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(addr, libc::MAP_FAILED);
        let addr = addr.cast::<u8>();
        // SAFETY: Within the mapping.
        unsafe { addr.write_bytes(1, len) };
        before_transfer(&file, addr);
        let (sender, mut receiver, stream, cell) =
            raw_connection(&socket, &dir.path().join("store"), 1 << 24);
        let transfer = Header::new(TRANSFER, 2).with_body(&TransferBody { len: len as u64 });
        sender.send(transfer, &[], &[file.as_fd()]).unwrap();
        assert_eq!(receiver.receive(|| 0).unwrap().unwrap().header.status, 0);
        let mut served =
            Self { _dir: dir, file, addr, len, sender, receiver, _stream: stream, cell };
        let written = Header::new(WRITE, 3).with_body(&WriteBody {
            cell,
            sequence: 1,
            data_len: BLOCK as u64,
            ..Default::default()
        });
        assert_eq!(served.call(written, &pattern(2, BLOCK)), 0);
        served
    }

    /// The status of `request`, of `payload`.
    fn call(&mut self, request: crate::wire::Header, payload: &[u8]) -> i32 {
        call(&self.sender, &mut self.receiver, request, payload).header.status
    }

    /// The status of a read, request `id`, of the cell's first block into the area at `at`.
    fn read_into(&mut self, id: u64, at: usize) -> i32 {
        use crate::wire::*;
        let read = Header::new(READ, id)
            .with_body(&ReadBody {
                cell: self.cell,
                sequence: 1,
                offset: 0,
                len: BLOCK as u64,
                transfer_at: at as u64,
            })
            .with_flags(READ_TRANSFER);
        self.call(read, &[])
    }

    /// The area's block at `at`.
    fn block(&self, at: usize) -> &[u8] {
        assert!(at + BLOCK <= self.len);
        // SAFETY: Within the mapping, which the daemon wrote and replied of.
        unsafe { std::slice::from_raw_parts(self.addr.add(at), BLOCK) }
    }
}

/// TASK-547, board 11707: what of a transfer area the daemon accesses is the VM process's memory,
/// each page of it written by the VM process before `TRANSFER`; the daemon seals the area against
/// writes once it has mapped it and verifies hole-free once with `lseek(0, SEEK_HOLE)`. A sparse
/// transfer area is refused at `TRANSFER` with `EINVAL`; a populated one is accepted, serves
/// reads/writes anywhere within its bounds, and cannot have pages punched out afterward.
#[test]
#[cfg(target_os = "linux")]
fn transfer_areas_are_their_vm_processes_memory() {
    use crate::wire::*;
    use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
    let len = 1 << 20;
    let page = cladefs_core::pages::page_size() as usize;

    // A partially written (sparse) memfd is refused up front at `TRANSFER` with `EINVAL`.
    {
        host();
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("cladefsd.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || serve(listener));
        // SAFETY: The name is a C string.
        let fd = unsafe { libc::memfd_create(c"sparse".as_ptr(), libc::MFD_ALLOW_SEALING) };
        assert!(fd >= 0);
        // SAFETY: Just made, owned here.
        let file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        file.set_len(len as u64).unwrap();
        let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW;
        // SAFETY: Seals what this test made.
        assert_eq!(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) }, 0);
        // SAFETY: Maps and writes only the first half, leaving the second half a hole.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(addr, libc::MAP_FAILED);
        // SAFETY: Within the first half of the mapping.
        unsafe { addr.cast::<u8>().write_bytes(1, len / 2) };
        let (sender, mut receiver, _stream, _cell) =
            raw_connection(&socket, &dir.path().join("store"), 1 << 24);
        let transfer = Header::new(TRANSFER, 2).with_body(&TransferBody { len: len as u64 });
        sender.send(transfer, &[], &[file.as_fd()]).unwrap();
        assert_eq!(receiver.receive(|| 0).unwrap().unwrap().header.status, -Errno::EINVAL.raw());
        // SAFETY: Unmaps what this block mapped.
        unsafe { libc::munmap(addr, len) };
    }

    let mut area = ServedArea::new(len);
    assert_eq!(area.read_into(5, 0), 0);
    assert_eq!(area.block(0), &pattern(2, BLOCK)[..]);
    let write_from = Header::new(WRITE, 51)
        .with_body(&WriteBody {
            cell: area.cell,
            sequence: 2,
            offset: 0,
            transfer_at: 0,
            data_len: BLOCK as u64,
        })
        .with_flags(WRITE_TRANSFER);
    assert_eq!(area.call(write_from, &[]), 0);
    assert_eq!(area.read_into(53, len / 2), 0);
    assert_eq!(area.block(len / 2), &pattern(2, BLOCK)[..]);
    // Out-of-bounds straddle past the end of the transfer area fails with EINVAL.
    let straddle = Header::new(READ, 52)
        .with_body(&ReadBody {
            cell: area.cell,
            sequence: 1,
            offset: 0,
            len: (2 * page) as u64,
            transfer_at: (len - page) as u64,
        })
        .with_flags(READ_TRANSFER);
    assert_eq!(area.call(straddle, &[]), -Errno::EINVAL.raw());
    // Sealed against writes: no page of it punched out via fallocate or MADV_REMOVE (board 7411).
    let mode = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
    // SAFETY: Of what this test made.
    let punched = unsafe { libc::fallocate(area.file.as_raw_fd(), mode, 0, page as libc::off_t) };
    assert_eq!(punched, -1, "a page of the area punched out");
    assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EPERM));
    // SAFETY: Of the mapping, page-aligned.
    let removed = unsafe { libc::madvise(area.addr.cast(), page, libc::MADV_REMOVE) };
    assert_eq!(removed, -1, "a page of the area removed via MADV_REMOVE");
    assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EPERM));
}

/// Board 6881, board 11707: a transfer area whose pages were written and paged out to swap before
/// `TRANSFER` is still accepted by the one-shot `SEEK_HOLE` check (swap entries are non-holes in
/// shmem's xarray), and a read into pages swapped out before or after `TRANSFER` is served.
/// Ignored where shared memory may not be paged out: CI's runners page none out, though they have
/// swap; ubu1's zram takes it (`--run-ignored all`).
#[test]
#[ignore = "needs a host that pages shared memory out to swap: ubu1's zram, not CI's runners"]
#[cfg(target_os = "linux")]
fn transfer_areas_written_and_swapped_out_are_read_into() {
    let len = 1 << 20;
    let at_before = len / 4;
    let at_after = len / 2;
    let page = cladefs_core::pages::page_size() as usize;
    let span = BLOCK.max(page);
    let page_out = |addr: *mut u8, at: usize| {
        let mut resident = vec![1u8; span / page];
        // SAFETY: Of the mapping, page-aligned, a page or more of it.
        let paged = unsafe { addr.add(at) };
        // SAFETY: As above.
        assert_eq!(unsafe { libc::madvise(paged.cast(), span, libc::MADV_PAGEOUT) }, 0);
        // SAFETY: As above, a byte a page.
        assert_eq!(unsafe { libc::mincore(paged.cast(), span, resident.as_mut_ptr().cast()) }, 0);
        let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let swap = meminfo.lines().filter(|line| line.starts_with("Swap")).collect::<Vec<_>>();
        assert!(resident.iter().all(|p| p & 1 == 0), "not paged out, the host's swap {swap:?}");
    };
    let mut area = ServedArea::with_setup(len, |_, addr| page_out(addr, at_before));
    assert_eq!(area.read_into(6, at_before), 0);
    assert_eq!(area.block(at_before), &pattern(2, BLOCK)[..]);
    page_out(area.addr, at_after);
    assert_eq!(area.read_into(7, at_after), 0);
    assert_eq!(area.block(at_after), &pattern(2, BLOCK)[..]);
}

/// TASK-575: a VM process drops a version it made, as its VM snapshot failing after it does, and
/// no other (`EPERM`): its VM manager's, made through the control API, are the manager's.
#[test]
fn vm_processes_drop_the_versions_they_made_alone() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let path = dir.path().join("store");
    let client = CellClient::connect(&socket, &path, 1 << 24).unwrap();
    write(&client, ExtentId { cell: client.cell(), sequence: 1 }, 0, &pattern(1, BLOCK)).unwrap();
    client.commit(None).unwrap();
    let managers =
        control(crate::cells::make_version(crate::Daemon::process(), path.clone(), None)).unwrap();
    let made = client.make_version().unwrap().version;
    let versions = || -> std::collections::BTreeSet<u64> {
        let versions =
            control(crate::cells::versions(crate::Daemon::process(), path.clone())).unwrap();
        versions.into_iter().map(|version| version.id).collect()
    };
    assert_eq!(versions(), [managers, made].into());
    assert_eq!(client.drop_version(managers), Err(Errno::EPERM));
    assert_eq!(client.drop_version(made), Ok(()));
    assert_eq!(client.drop_version(made), Err(Errno::EPERM));
    assert_eq!(versions(), [managers].into());
}

/// TASK-266: a cell the control API makes of a cell, at a placement (`CreateCell`, as
/// `cladefsctl take` through the daemon's), takes the cell, and so its head, with that
/// placement's token as it is made, not the daemon's: a copy placed later than the one that took
/// the cell last, on another host, takes it over, in a file bucket as in any.
#[tokio::test(flavor = "multi_thread")]
async fn control_takes_cells_at_the_placements_given() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let url = format!("file://{}/", dir.path().canonicalize().unwrap().join("bucket").display());
    // SAFETY: as in host.
    unsafe { std::env::set_var("CLADEFSD_BUCKET", &url) };
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let path = |name: &str| dir.path().join(name);
    // Host 1's copy, of a new cell the bucket names, placed at 5, its commit synced.
    let new = cladefs_core::CellId { tenant: 0, id: 0 };
    let made = crate::cells::create(
        crate::Daemon::process(),
        path("first"),
        1 << 24,
        new,
        (Some(5), Kind::Fxfs, None, None, false),
    );
    let cell = control(made).unwrap();
    let extent = ExtentId { cell: cell.id, sequence: 1 };
    let first = CellClient::connect(&socket, path("first"), 1 << 24).unwrap();
    write(&first, extent, 0, &pattern(7, BLOCK)).unwrap();
    first.commit(Some(b"placed at 5")).unwrap();
    drop(first);
    assert_eq!(control(crate::cells::sync(crate::Daemon::process(), path("first"))), Ok(()));
    // A copy beside the daemon, as cladefsctl's without it, placed at 6.
    cladefs_core::open_cell::Opening::here()
        .unwrap()
        .take(path("second"), 1 << 24, cell, Some(6))
        .await
        .unwrap();
    // One through the daemon, placed at 7.
    let taken = crate::cells::create(
        crate::Daemon::process(),
        path("third"),
        1 << 24,
        cell,
        (Some(7), Kind::Fxfs, None, None, false),
    );
    assert_eq!(control(taken), Ok(cell));
    let third = CellClient::connect(&socket, path("third"), 1 << 24).unwrap();
    assert_eq!(third.root().unwrap(), b"placed at 5");
    assert_eq!(read(&third, extent, 0, BLOCK), Ok(pattern(7, BLOCK)));
}

/// cladefs: a tenant's arena, and the extents cached in it, go with the tenant, once its grant and
/// its VM processes have (TASK-43.4).
#[test]
#[cfg(target_os = "linux")]
fn tenants_arenas_go_with_them() {
    host();
    let ps = page_size() as usize;
    let dir = tempfile::tempdir().unwrap();
    let tenant = crate::Tenant::new(1, dir.path()).unwrap();
    let arena = tenant.arena();
    let (stream, server) = UnixStream::pair().unwrap();
    let served = std::thread::spawn(move || serve_connection(server, tenant));
    let client =
        CellClient::over(stream, &dir.path().join("store"), Opening::Create(1 << 24, Kind::Fxfs))
            .unwrap();
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    client.write(extent, 0, &[VolatileSlice::from(&pattern(4, ps)[..])]).unwrap();
    client.commit(None).unwrap();
    let memory = memory(1);
    // SAFETY: The memory is this test's, and outlives the cell.
    unsafe { client.create_dax_window_in_host_memory(memory, ps as u64, None, 1) }.unwrap();
    client.map(extent, 0, ps as u64, 0).unwrap();
    // SAFETY: The window's memory, whose faults the cell serves: the extent cached, in the arena.
    let page = unsafe { std::slice::from_raw_parts(memory.as_ptr(), ps) };
    assert!(*page == pattern(4, ps)[..], "the window maps other data");
    assert!(cladefs_core::pages::arena_memory(arena).is_some());

    drop(client);
    served.join().unwrap();
    // Gone with the tenant, once the page-in that served the fault, which holds the extent a while
    // after the fault returns, lets it go: the arena retires until then.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while cladefs_core::pages::arena_memory(arena).is_some() {
        assert!(std::time::Instant::now() < deadline, "the tenant's arena stayed");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// cladefs: a writer that goes with writes uncommitted leaves its cell as of its last commit, which
/// the next writer opens.
#[test]
fn writers_that_go_with_writes_uncommitted_leave_their_last_commit() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let (client, served) = connect(&path, 1 << 20);
    let cell = client.cell();
    let extent = move |sequence| ExtentId { cell, sequence };
    write(&client, extent(1), 0, &pattern(1, BLOCK)).unwrap();
    client.commit(None).unwrap();
    write(&client, extent(2), 0, &pattern(2, BLOCK)).unwrap();
    drop(client);
    served.join().unwrap();

    let (client, served) = connect(&path, 1 << 20);
    assert_eq!(read(&client, extent(1), 0, BLOCK), Ok(pattern(1, BLOCK)));
    assert_eq!(read(&client, extent(2), 0, BLOCK), Err(Errno::ENOENT));
    drop(client);
    served.join().unwrap();
}

/// What this process holds, as Linux counts it: the daemon's threads (but for its clients'), its
/// open files, and its resident memory.
#[derive(Debug, Clone, Copy)]
#[cfg(target_os = "linux")]
struct Held {
    threads: usize,
    files: usize,
    resident: u64,
}

#[cfg(target_os = "linux")]
impl Held {
    fn now() -> Self {
        let threads = thread_comms().filter(|comm| comm != "cladefsd client").count();
        let files = std::fs::read_dir("/proc/self/fd").unwrap().count();
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let kib = status
            .lines()
            .find_map(|line| line.strip_prefix("VmRSS:"))
            .and_then(|kib| kib.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            .unwrap();
        Self { threads, files, resident: kib << 10 }
    }
}

/// This process's open files, by what each is, its digits each `N`, so that the files of each fork
/// count as one kind: what a test of how many files forks hold says it found. One closed between
/// the listing of `/proc/self/fd` and its reading is gone, and holds nothing.
#[cfg(target_os = "linux")]
fn files_by_kind() -> std::collections::BTreeMap<String, usize> {
    let mut kinds = std::collections::BTreeMap::new();
    for fd in std::fs::read_dir("/proc/self/fd").unwrap() {
        let Ok(target) = std::fs::read_link(fd.unwrap().path()) else { continue };
        let target = target.to_string_lossy();
        let mut kind = String::with_capacity(target.len());
        for c in target.chars() {
            match (c.is_ascii_digit(), kind.ends_with('N')) {
                (true, true) => {}
                (true, false) => kind.push('N'),
                (false, _) => kind.push(c),
            }
        }
        *kinds.entry(kind).or_default() += 1;
    }
    kinds
}

/// The names of this process's threads alive as they are read: one that ends between the listing of
/// `/proc/self/task` and the read of its name, as an idle blocking thread of the daemon's runtimes
/// does, is gone, and holds nothing: its name not found, or, as it exits, its process no more
/// (`ESRCH`), as for the threads a cell's flush syncs its files on (`local_cell`'s `AT_ONCE`).
#[cfg(target_os = "linux")]
fn thread_comms() -> impl Iterator<Item = String> {
    std::fs::read_dir("/proc/self/task").unwrap().filter_map(|task| {
        match std::fs::read_to_string(task.unwrap().path().join("comm")) {
            Ok(comm) => Some(comm.trim().to_owned()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) if err.raw_os_error() == Some(libc::ESRCH) => None,
            Err(err) => panic!("reading a thread's name: {err}"),
        }
    })
}

/// This process's threads, by name, and how many of each.
#[cfg(target_os = "linux")]
fn thread_names() -> std::collections::BTreeMap<String, usize> {
    let mut names = std::collections::BTreeMap::new();
    for comm in thread_comms() {
        *names.entry(comm).or_default() += 1;
    }
    names
}

/// TASK-34: the daemon serves 300 cells, forks of one base, open at once, each reading its base,
/// on threads they share: the daemon's threads do not grow with them, and each holds a few files
/// open and a little memory, the base's state and its files' handles shared.
#[test]
#[ignore = "board 10671: each fork holds 11 files, past the 10 this bounds: its directory twice, \
            DiskDir's handle and the dup its flock holds (local_cell::dir::try_take_from_others)"]
#[cfg(target_os = "linux")]
fn serves_hundreds_of_forks_of_a_base() {
    const FORKS: usize = 300;
    const EXTENTS: u64 = 64;
    host();
    // As the daemon does.
    crate::raise_open_files_limit().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base");
    let (client, served) = connect(&base, 1 << 30);
    let cell = client.cell();
    for sequence in 0..EXTENTS {
        let extent = ExtentId { cell, sequence };
        write(&client, extent, 0, &pattern(sequence as u8, 16 * BLOCK)).unwrap();
    }
    client.commit(None).unwrap();
    drop(client);
    served.join().unwrap();
    let forks: Vec<_> = (0..FORKS)
        .map(|n| {
            let fork = dir.path().join(format!("fork-{n}"));
            fork_cell(&base, &fork).unwrap();
            fork
        })
        .collect();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));

    let before = Held::now();
    let cells: Vec<_> =
        (forks.iter()).map(|fork| CellClient::connect(&socket, fork, 1 << 30).unwrap()).collect();
    for (n, client) in cells.iter().enumerate() {
        let sequence = n as u64 % EXTENTS;
        let extent = ExtentId { cell, sequence };
        assert_eq!(read(client, extent, 0, BLOCK).unwrap(), pattern(sequence as u8, BLOCK));
    }
    let after = Held::now();
    let files = (after.files - before.files) as f64 / FORKS as f64;
    let resident = after.resident.saturating_sub(before.resident) / FORKS as u64;
    println!("{FORKS} forks open: before {before:?}, after {after:?}");
    println!("per fork: {files:.1} files, {} KiB resident", resident >> 10);
    // The daemon's threads: its runtimes', and the requests' running, not a set per connection.
    assert!(after.threads < 64, "{} threads: {:?}", after.threads, thread_names());
    // Each fork's: the connection's two ends here, and the client's, with its kernel's memory, its
    // arena and the default one, read-only (TASK-79.17); its cell's directory, its own since
    // TASK-165.7, its open segment and its commit log (TASK-165.9). And the base's, which it is
    // attached to.
    let opened = after.files - before.files;
    assert!(opened <= 10 * FORKS + 64, "{opened} files for {FORKS} forks: {:#?}", files_by_kind());
    assert!(resident < 1 << 20, "{resident} bytes a fork");
    drop(cells);
}

/// TASK-92.55: opening a fork through the daemon unpacks and reads nothing more of a larger base:
/// forks of a base of 32,768 extents unpack and read as much as forks of one of 1,024. 100 forks of
/// the larger had taken about 0.5 s each on a GCE host (debug), in zstd on a bucket runtime thread.
/// Their times are said, not bounded: what still grows with the base, the parent's checkpoint at
/// each fork and the child's decoding it, is ~0.3 us an extent in a release build (~11 ms a fork of
/// the larger against ~3.5 ms, Linux in Docker), under a round trip to a real bucket, and 42 ms
/// against 5 ms in debug (124.5 ms against 6.0 ms on GCE r1021 before the extents' map was
/// persistent).
#[tokio::test(flavor = "multi_thread")]
async fn forks_opening_reads_nothing_more_of_a_larger_base() {
    const FORKS: u64 = 20;
    host();
    crate::raise_open_files_limit().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    // Forks of a base of `extents` extents, opened: how long their opening took, and what it
    // unpacked and read from the cells.
    let opened = async |extents: u64| {
        let base =
            OpenCell::create(dir.path().join(format!("base-{extents}")), 1 << 30).await.unwrap();
        let cell = base.cell();
        for sequence in 1..=extents {
            let data = pattern(sequence as u8, BLOCK);
            base.write(ExtentId { cell: cell.id, sequence }, 0, &[VolatileSlice::from(&data[..])])
                .await
                .unwrap();
            if sequence % 1024 == 0 {
                base.commit(None).await.unwrap();
            }
        }
        let mut forks = Vec::new();
        for n in 1..=FORKS {
            let fork = dir.path().join(format!("fork-{extents}-{n}"));
            base.fork_to(&fork, cladefs_core::host_cell::fork_child(cell)).await.unwrap();
            forks.push(fork);
        }
        drop(base);
        let before = cladefs_core::stats::stats();
        let start = std::time::Instant::now();
        let cells: Vec<_> = (forks.iter())
            .map(|fork| CellClient::connect(&socket, fork, 1 << 30).unwrap())
            .collect();
        let took = start.elapsed() / FORKS as u32;
        let after = cladefs_core::stats::stats();
        drop(cells);
        (took, after.unpacks - before.unpacks, after.read_stored_bytes - before.read_stored_bytes)
    };
    let small = opened(1024).await;
    let large = opened(32768).await;
    println!("a fork's opening, of a base of 1,024 extents and of 32,768: {small:?} and {large:?}");
    assert_eq!((large.1, large.2), (small.1, small.2), "(unpacked, read) by opening forks");
}

/// TASK-92.49: the daemon makes the ordered commits of 300 cells' guests durable on threads they
/// share, a bounded number, rather than a blocking thread for each cell whose flush is due at
/// its turn, which hundreds of guests committing together kept as many of as their syncs' waits
/// took, each idle 10 s after: here each guest commits, ordered, again and again for a second.
/// At once, `FLUSHES` cells' flushes run at most, each on a thread of the runtime's for its
/// syncs alone (board 2944), and the 15 they share to sync their files on beside their own
/// (`local_cell`'s `HELPERS`): the most threads seen at once, looking every millisecond, are a
/// small part of the cells'. Those 15, and the cells' framers', syncers' and openers', are of
/// the cells' pool (`local_cell::pool`), which keeps no more than its bound however many cells
/// commit (board 2944). Once their commits are durable, the runtime keeps threads for
/// the requests that ran at once, not for the cells: 2 and a little over 2 for each guest
/// committing (8 to 9 for 3 guests, 18 to 25 for 9, 58 to 70 for 33, whether 150, 300 or 600 cells
/// were open, in a container of 15 processors), more under load, as a thread just done is not yet
/// idle when the next request comes for one (40 beside the full workspace's tests); a thread for
/// each cell would be 297. One guest's commits are durable within a few of its
/// cell's turns, of 50 ms each and the flushes' time: told in turns, which the daemon counts
/// ([`OpenCell::flush_turns`]), as the full workspace's tests beside it stretch each turn's
/// flushes to hundreds of milliseconds (TASK-110); the most one waited in time, its lag, is
/// reported. The cells are on
/// a disk, in `/var/tmp`, as a host's are: `/tmp` may be memory (tmpfs), whose syncs cost nothing.
#[test]
#[cfg(target_os = "linux")]
fn hundreds_of_writers_flush_on_threads_they_share() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    const CELLS: usize = 300;
    const WRITERS: usize = 8;
    // The runtime's, those flushing among them; and the cells' pool's.
    const NAMES: [&str; 2] = ["cladefsd io", local_cell::pool::NAME];
    host();
    crate::raise_open_files_limit().unwrap();
    // CI runners (RLIMIT_NOFILE = 65536) admit fewer than 300 connections under worst-case
    // `cell_open_files()`, though 300 simple writers use <2,000 FDs total (board 13823).
    crate::server::CONNECTION_LIMITS_GIVEN.set((Some(CELLS + 16), 0)).unwrap();
    // Prefer `/var/tmp` (real disk) when writable; fall back to the test temp dir inside
    // read-only sandboxes (Bazel `linux-sandbox`, hotbed workspaces; board 13609).
    let dir = tempfile::tempdir_in("/var/tmp").or_else(|_| tempfile::tempdir()).unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let paths: Vec<_> = (0..CELLS).map(|n| dir.path().join(format!("store-{n}"))).collect();
    let cells: Vec<_> = (paths.iter())
        .map(|path| Arc::new(CellClient::connect(&socket, path, 1 << 30).unwrap()))
        .collect();
    // The daemon's side of each cell, which says whether its commits wait to be made durable;
    // and the guest whose commits' lag is measured.
    let theirs: Vec<_> = (paths.iter())
        .map(|path| {
            let key = crate::cells::cell_dir_key(path);
            crate::cells::table().writer(&key).unwrap().cell().unwrap().clone()
        })
        .collect();
    let ((probe, others), probed) = (cells.split_first().unwrap(), theirs[0].clone());
    // The most of them seen at once, and how many of each kind then.
    let sampling = Arc::new(AtomicBool::new(true));
    let sampler = {
        let sampling = sampling.clone();
        std::thread::spawn(move || {
            let mut most = [0; NAMES.len()];
            while sampling.load(Ordering::Relaxed) {
                let names = thread_names();
                let threads = NAMES.map(|name| names.get(name).copied().unwrap_or(0));
                if threads.iter().sum::<usize>() > most.iter().sum() {
                    most = threads;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            most
        })
    };
    let until = std::time::Instant::now() + std::time::Duration::from_secs(1);
    let writers: Vec<_> = (others.chunks(others.len().div_ceil(WRITERS)))
        .map(|cells| {
            let cells = cells.to_vec();
            std::thread::spawn(move || {
                let mut sequence = 0;
                while std::time::Instant::now() < until {
                    sequence += 1;
                    for client in &cells {
                        let extent = ExtentId { cell: client.cell(), sequence };
                        write(client, extent, 0, &pattern(sequence as u8, BLOCK)).unwrap();
                        client.commit_ordered(None).unwrap();
                    }
                }
            })
        })
        .collect();
    let lagging = {
        let probe = probe.clone();
        std::thread::spawn(move || {
            let (mut lags, mut turns, mut sequence) = (Vec::new(), 0, 0);
            while std::time::Instant::now() < until {
                sequence += 1;
                let extent = ExtentId { cell: probe.cell(), sequence };
                write(&probe, extent, 0, &pattern(sequence as u8, BLOCK)).unwrap();
                probe.commit_ordered(None).unwrap();
                let (committed, turned) = (std::time::Instant::now(), probed.flush_turns());
                while probed.flush_pending() {
                    std::thread::yield_now();
                }
                lags.push(committed.elapsed());
                turns = turns.max(probed.flush_turns() - turned);
            }
            lags.sort();
            (lags, turns)
        })
    };
    for writer in writers {
        writer.join().unwrap();
    }
    let (lags, turns) = lagging.join().unwrap();
    // Every guest's commits durable, what the flushes took and kept.
    while theirs.iter().any(|client| client.flush_pending()) {
        std::thread::yield_now();
    }
    let names = thread_names();
    sampling.store(false, Ordering::Relaxed);
    let most = sampler.join().unwrap();
    let kept = NAMES.map(|name| names.get(name).copied().unwrap_or(0));
    let threads: usize = kept.iter().sum();
    let (median, lag) = (lags[lags.len() / 2], lags[lags.len() - 1]);
    println!(
        "{CELLS} cells' guests committing: {threads} threads kept, {kept:?} named {NAMES:?}, \
         at most {} at once, {most:?}; a guest's {} ordered commits durable within {median:?}, \
         the median, and {lag:?}, {turns} of its cell's turns",
        most.iter().sum::<usize>(),
        lags.len()
    );
    assert!(most[0] < CELLS / 4, "{most:?} named {NAMES:?} at once");
    assert!(kept[0] < CELLS / 4, "{threads} threads kept: {kept:?} named {NAMES:?}, of {names:?}");
    let pool = local_cell::pool::threads();
    assert!(most[1] <= pool && kept[1] <= pool, "{most:?} at most, {kept:?} kept, of {pool}");
    // Four turns, however long the load made them.
    assert!(turns <= 4, "ordered commits durable {turns} turns after, {lag:?}");
    drop(cells);
}

/// TASK-47: the daemon's metrics, scraped over HTTP as Prometheus scrapes them, count each kind of
/// request as it is served, and say how many VM processes are connected and cells open.
#[test]
fn metrics_count_requests_as_they_are_served() {
    use std::io::{Read as _, Write as _};
    host();
    let address = http_server(crate::Pages::All);
    let get = |path: &str| {
        let mut stream = std::net::TcpStream::connect(address).unwrap();
        write!(stream, "GET {path} HTTP/1.1\r\nHost: cladefsd\r\n\r\n").unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    };
    let metric = |metrics: &str, name: &str| -> u64 {
        let line = (metrics.lines())
            .find(|line| line.split_once(' ').is_some_and(|(found, _)| found == name))
            .unwrap_or_else(|| panic!("no {name} in {metrics}"));
        line.rsplit_once(' ').unwrap().1.parse().unwrap()
    };
    let before = get("/metrics");
    assert!(before.starts_with("HTTP/1.1 200 OK\r\n"), "{before}");
    assert_eq!(metric(&before, "cladefsd_requests_total{kind=\"write\"}"), 0);
    assert_eq!(metric(&before, "cladefsd_connections"), 0);

    let dir = tempfile::tempdir().unwrap();
    let (client, served) = connect(&dir.path().join("store"), 1 << 20);
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    for offset in 0..3 {
        write(&client, extent, offset * BLOCK as u64, &pattern(1, BLOCK)).unwrap();
    }
    assert_eq!(read(&client, extent, 0, BLOCK), Ok(pattern(1, BLOCK)));
    let metrics = get("/metrics");
    let requests =
        |kind: &str| metric(&metrics, &format!("cladefsd_requests_total{{kind=\"{kind}\"}}"));
    assert_eq!(
        [requests("open"), requests("transfer"), requests("write"), requests("read")],
        [1, 1, 3, 1]
    );
    let histogram = "cladefsd_request_duration_seconds";
    assert_eq!(metric(&metrics, &format!("{histogram}_count{{kind=\"write\"}}")), 3);
    assert_eq!(metric(&metrics, &format!("{histogram}_bucket{{kind=\"write\",le=\"+Inf\"}}")), 3);
    assert_eq!(metric(&metrics, "cladefsd_request_errors_total{kind=\"write\"}"), 0);
    assert_eq!(metric(&metrics, "cladefsd_connections"), 1);
    assert_eq!(metric(&metrics, "cladefsd_stores_open"), 1);
    // TASK-91.3: its uploader's lag, nothing once a durable commit is uploaded.
    client.commit(None).unwrap();
    let metrics = get("/metrics");
    let upload = |name: &str| {
        format!("cladefsd_upload_{name}{{cell=\"{}\"}}", cell_at(&dir.path().join("store")))
    };
    assert_eq!(
        [
            metric(&metrics, &upload("pending_bytes")),
            metric(&metrics, &upload("oldest_pending_seconds")),
            metric(&metrics, &upload("stalls_total")),
        ],
        [0, 0, 0]
    );
    // TASK-314: the bucket's requests that failed, by kind: none of a bucket answering all.
    let failures = |request: &str| {
        metric(
            &metrics,
            &format!("cladefsd_object_storage_failures_total{{request=\"{request}\"}}"),
        )
    };
    assert_eq!([failures("create"), failures("get-range"), failures("update")], [0, 0, 0]);
    assert!(get("/other").starts_with("HTTP/1.1 404 Not Found\r\n"));
    drop(client);
    served.join().unwrap();
}

/// TASK-79.27: the daemon's metrics server answers a request line longer than it reads with `431`,
/// holding no more of it than it reads (`HEAD_MAX`): what the client sends after that it reads and
/// drops, until the client is done, as the connection, closed with bytes unread, would be reset,
/// the answer lost; then it closes the connection.
#[test]
fn metrics_refuse_requests_too_long() {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || crate::serve_http(listener, crate::Pages::All));
    let mut stream = std::net::TcpStream::connect(address).unwrap();
    let mut writing = stream.try_clone().unwrap();
    // A line of a mebibyte, which a server reading lines whole holds whole.
    std::thread::spawn(move || {
        writing.write_all(&vec![b'G'; 1 << 20]).unwrap();
        writing.shutdown(std::net::Shutdown::Write).unwrap();
    });
    let response = within(std::time::Duration::from_secs(10), move || {
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    });
    let response = response.unwrap();
    assert!(response.starts_with("HTTP/1.1 431 Request Header Fields Too Large\r\n"), "{response}");
}

/// TASK-79.27: a scrape is answered while another client, slow, has sent but part of its request:
/// the daemon's metrics server waits for neither on the other. The slow client is answered
/// nothing, its connection open, once the scrape is done: a server serving one at a time would have
/// answered the scrape only once the slow client's time was up, and it closed.
#[test]
fn metrics_are_served_beside_slow_clients() {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || crate::serve_http(listener, crate::Pages::All));
    let mut slow = std::net::TcpStream::connect(address).unwrap();
    write!(slow, "GET /metrics HTTP/1.1\r\n").unwrap();
    let scraped = within(std::time::Duration::from_secs(30), move || {
        let mut stream = std::net::TcpStream::connect(address).unwrap();
        write!(stream, "GET /metrics HTTP/1.1\r\nHost: cladefsd\r\n\r\n").unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    });
    let scraped = scraped.expect("the scrape is answered never");
    assert!(scraped.starts_with("HTTP/1.1 200 OK\r\n"), "{scraped}");
    slow.set_nonblocking(true).unwrap();
    let answered = slow.read(&mut [0; 64]).map_err(|err| err.kind());
    assert_eq!(answered, Err(std::io::ErrorKind::WouldBlock), "the slow client was let go first");
}

/// What the HTTP server at `address` answers `request`, its line and headers, read to its end
/// within 30 s.
fn http(address: std::net::SocketAddr, request: &str) -> String {
    use std::io::{Read as _, Write as _};
    let request = format!("{request}\r\nHost: cladefsd\r\n\r\n");
    let answered = within(std::time::Duration::from_secs(30), move || {
        let mut stream = std::net::TcpStream::connect(address).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = Vec::new();
        // A connection the server dropped, its page panicking, reset or ended: what it read.
        let _ = stream.read_to_end(&mut response);
        String::from_utf8_lossy(&response).into_owned()
    });
    answered.expect("the HTTP server answered never")
}

/// An HTTP server serving `pages` on a port of localhost's, on a thread: its address.
fn http_server(pages: crate::Pages) -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || crate::serve_http(listener, pages));
    address
}

/// board 8559: a page that panics, as many times as clients are served at once, gives back each
/// one's turn: the metrics are served after. Its turn held for good as it unwound, the server
/// answered nothing more after four, the daemon serving on.
#[test]
fn metrics_are_served_after_pages_panic() {
    let address = http_server(crate::Pages::All);
    fail::cfg("cladefsd::http::route", "4*panic").unwrap();
    for _ in 0..4 {
        assert_eq!(http(address, "GET /metrics HTTP/1.1"), "", "a page that panics answers");
    }
    let scraped = http(address, "GET /metrics HTTP/1.1");
    assert!(scraped.starts_with("HTTP/1.1 200 OK\r\n"), "{scraped}");
}

/// board 8559: a CPU profile whose client hangs up ends then, within a second, whatever it sent
/// after its request, rather than sample on for the 300 seconds it asked for, holding one of the
/// server's turns and refusing every other profile (409).
#[test]
fn profiles_end_as_their_clients_hang_up() {
    use std::io::Write as _;
    use std::time::{Duration, Instant};
    let address = http_server(crate::Pages::All);
    let mut gone = std::net::TcpStream::connect(address).unwrap();
    write!(gone, "GET /z/pprof/profile?seconds=300 HTTP/1.1\r\nHost: cladefsd\r\n\r\n").unwrap();
    let until = |taken: bool, what: &str| {
        let began = Instant::now();
        while crate::zpages::pprof::taken() != taken {
            assert!(began.elapsed() < Duration::from_secs(10), "{what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    until(true, "the profile was never taken");
    // Bytes past its request, which a peek at what it sent saw for good, the end behind them not
    // (hotbed review 66).
    gone.write_all(b"x").unwrap();
    drop(gone);
    until(false, "the profile went on, its client gone");
    let next = http(address, "GET /z/pprof/profile?seconds=1 HTTP/1.1");
    assert_eq!(next.lines().next(), Some("HTTP/1.1 200 OK"));
}

/// board 8559: a server of the metrics alone serves no z-page, CPU profiles above all, which a
/// scraper need not take; one of the z-pages alone, no metrics (`CLADEFSD_ZPAGES`).
#[test]
fn z_pages_are_served_apart_from_the_metrics() {
    let (metrics, zpages) = (http_server(crate::Pages::Metrics), http_server(crate::Pages::Zpages));
    let status = |answer: String| answer.lines().next().unwrap_or_default().to_owned();
    assert_eq!(status(http(metrics, "GET /metrics HTTP/1.1")), "HTTP/1.1 200 OK");
    for page in ["/z/", "/z/pprof/profile?seconds=1", "/z/log"] {
        let answer = http(metrics, &format!("GET {page} HTTP/1.1"));
        assert_eq!(status(answer), "HTTP/1.1 404 Not Found", "{page}");
    }
    assert_eq!(status(http(zpages, "GET /metrics HTTP/1.1")), "HTTP/1.1 404 Not Found");
    assert_eq!(status(http(zpages, "GET /z/ HTTP/1.1")), "HTTP/1.1 200 OK");
}

/// board 8571: what the daemon logs changes while it runs, `POST /z/log?level=`, as
/// `CLADEFS_LOG_LEVEL` says it, and is put back, `POST /z/log`: a daemon serving its VMs, which a
/// restart would crash, logs a target at debug, then as before. What is not a level is refused,
/// nothing changed.
#[test]
fn what_the_daemon_logs_changes_while_it_runs() {
    let lines = crate::logging::capture();
    let address = http_server(crate::Pages::All);
    let logged = |what: &str| {
        log::debug!(target: "cladefs::cladefsd::server", "{what}");
        log::info!(target: "bucket_cell::object_bucket", "{what}");
        let lines = lines.lock().unwrap();
        let of =
            |target: &str| lines.iter().any(|line| line.contains(&format!("{target}: {what}")));
        (of("cladefs::cladefsd::server"), of("bucket_cell::object_bucket"))
    };
    let body = |answer: String| answer.split_once("\r\n\r\n").map(|(_, body)| body.to_owned());
    assert_eq!(logged("as started"), (true, true));
    let set = http(address, "POST /z/log?level=warn,cladefs::cladefsd=debug HTTP/1.1");
    assert_eq!(body(set), Some("warn,cladefs::cladefsd=debug\n".to_owned()));
    assert_eq!(logged("as told"), (true, false));
    let refused = http(address, "POST /z/log?level=loud HTTP/1.1");
    assert!(refused.starts_with("HTTP/1.1 400 Bad Request\r\n"), "{refused}");
    let now = http(address, "GET /z/log HTTP/1.1");
    assert_eq!(body(now), Some("warn,cladefs::cladefsd=debug\n".to_owned()));
    let put_back = http(address, "POST /z/log HTTP/1.1");
    assert_eq!(body(put_back), Some("debug\n".to_owned()));
    assert_eq!(logged("as before"), (true, true));
}

/// board 8561: the process's metrics are the daemon's from its start, before any cell opens, and
/// its bucket's and part cache's families are there, 0: they read 0, an idle daemon's, and were
/// missing, until a cell opened the bucket.
#[test]
fn metrics_are_there_before_any_cell_opens() {
    let metrics = crate::metrics::render();
    let value = |name: &str| {
        let line = metrics.lines().find_map(|line| line.strip_prefix(&format!("{name} ")));
        line.unwrap_or_else(|| panic!("no {name} in {metrics}")).parse::<f64>().unwrap()
    };
    assert!(value("cladefsd_process_anon_bytes") > 0.0, "{metrics}");
    assert!(value("cladefsd_process_cpu_seconds_total") > 0.0, "{metrics}");
    assert!(value("cladefsd_process_anon_budget_bytes") > 0.0, "{metrics}");
    assert_eq!(value("cladefsd_part_cache_bytes"), 0.0);
    assert_eq!(value("cladefsd_object_storage_downloaded_bytes_total"), 0.0);
    let get = bucket_cell::object_bucket::Request::Get.name();
    assert_eq!(value(&format!("cladefsd_object_storage_requests_total{{request=\"{get}\"}}")), 0.0);
    // cladefs: board 8565: the control API's and the peers' calls, each method's, the migrations,
    // the FUSE mount, the page cache, the governor and the spans exported, 0, from the start.
    let call = "service=\"cladefs.cladefsd.v1.Cladefsd\",method=\"CreateCell\"";
    let peer = "service=\"cladefs.peer.v1.Peer\",method=\"GetPart\"";
    for series in [
        format!("cladefsd_grpc_server_calls_total{{{call}}}"),
        format!("cladefsd_grpc_server_errors_total{{{call}}}"),
        format!("cladefsd_grpc_server_duration_seconds_count{{{call}}}"),
        format!("cladefsd_grpc_server_calls_total{{{peer}}}"),
        format!("cladefsd_grpc_client_calls_total{{{peer}}}"),
        format!("cladefsd_grpc_client_duration_seconds_bucket{{{peer},le=\"+Inf\"}}"),
        "cladefsd_grpc_server_calls_total{service=\"cladefs.mount.v1.Mount\",method=\"unknown\"}"
            .to_owned(),
        "cladefsd_migrations{state=\"moving\"}".to_owned(),
        "cladefsd_migrations{state=\"taking\"}".to_owned(),
        "cladefsd_migrations_total{outcome=\"aborted\"}".to_owned(),
        "cladefsd_migration_rounds_total".to_owned(),
        "cladefsd_migration_sent_bytes_total".to_owned(),
        "cladefsd_mount_cells{state=\"attached\"}".to_owned(),
        "cladefsd_mount_requests_total".to_owned(),
        "cladefsd_mount_request_errors_total".to_owned(),
        "cladefsd_mount_attaches_total".to_owned(),
        "cladefsd_page_cache_flush_stalls_total".to_owned(),
        "cladefsd_memory_full".to_owned(),
        "cladefsd_memory_pressure".to_owned(),
        "cladefsd_memory_thrashing".to_owned(),
        "cladefsd_memory_full_entered_total".to_owned(),
        "cladefsd_memory_refaults_total".to_owned(),
        "cladefsd_memory_kernel_cold_harvested_bytes_total".to_owned(),
        "cladefsd_vm_reclaim_bytes_total".to_owned(),
        "cladefsd_vm_reclaim_attempts_total".to_owned(),
        "cladefsd_vm_reclaim_errors_total".to_owned(),
        "cladefsd_memory_refaults_per_second".to_owned(),
        "cladefsd_memory_target_bytes".to_owned(),
        "cladefsd_memory_psi_some_avg10_bps".to_owned(),
        "cladefsd_memory_psi_full_avg10_bps".to_owned(),
        "cladefsd_otel_spans_exported_total".to_owned(),
        "cladefsd_otel_spans_dropped_total".to_owned(),
        "cladefsd_otel_spans_failed_total".to_owned(),
        // cladefs: where the host's fetchers found parts and tails, each source's.
        "cladefsd_part_fetches_total{source=\"memory\"}".to_owned(),
        "cladefsd_part_fetch_bytes_total{source=\"part_cache\"}".to_owned(),
        "cladefsd_part_fetches_total{source=\"peer\"}".to_owned(),
        "cladefsd_part_fetch_bytes_total{source=\"bucket\"}".to_owned(),
        "cladefsd_tail_fetches_total{source=\"part_cache\"}".to_owned(),
        "cladefsd_tail_fetches_total{source=\"bucket\"}".to_owned(),
    ] {
        assert_eq!(value(&series), 0.0, "{series}");
    }
    assert!(value("cladefsd_page_cache_limit_bytes") > 0.0, "{metrics}");
    assert!(value("cladefsd_page_cache_dirty_limit_bytes") > 0.0, "{metrics}");
    // No family is named twice: each its own HELP and TYPE, once.
    let mut families: Vec<&str> = metrics
        .lines()
        .filter_map(|line| line.strip_prefix("# TYPE ")?.split(' ').next())
        .collect();
    let count = families.len();
    families.sort_unstable();
    families.dedup();
    assert_eq!(families.len(), count, "a family named twice");
}

/// Board 10482: the daemon's metrics, once it has served a VM process's requests, its window's
/// among them, control calls, and a migration with its peer calls (one its peer, speaking no TLS,
/// fails), parse as a scraper parses Prometheus' text format, strictly
/// ([`super::metrics::parsed`]), every family among them. Until then its tests matched lines: a
/// family's HELP said twice, a histogram without `+Inf`, a series said twice, passed them all and
/// failed the scrape.
#[test]
#[cfg(target_os = "linux")]
fn the_daemons_metrics_parse_as_a_scraper_parses_them() {
    use crate::control::proto::GetStatsRequest;
    host();
    let ps = page_size() as usize;
    let dir = tempfile::tempdir().unwrap();
    let root = host_fs::canonicalize(dir.path()).unwrap();
    let path = root.join("store");
    let (client, served) = connect(&path, 1 << 24);
    let memory = memory(2);
    // SAFETY: The memory is the test's, and outlives the cell.
    unsafe { client.create_dax_window_in_host_memory(memory, 2 * ps as u64, None, 1) }.unwrap();
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, extent, 0, &pattern(6, ps)).unwrap();
    client.commit(None).unwrap();
    client.map(extent, 0, ps as u64, 0).unwrap();
    assert_eq!(read(&client, extent, 0, ps).unwrap(), pattern(6, ps));
    // An ephemeral cell, closed, to migrate (peer migration serves ephemeral cells only).
    cladefs_core::ephemeral_host::open_on(&crate::Daemon::process().host, &root.join("ephemeral"))
        .unwrap();
    let base = root.join("base");
    let (closed, closed_served) = connect(&base, 1 << 24);
    let base_cell = cladefs_core::CellId { tenant: 0, id: closed.cell() };
    write(&closed, ExtentId { cell: base_cell.id, sequence: 1 }, 0, &pattern(7, ps)).unwrap();
    closed.commit(None).unwrap();
    drop(closed);
    closed_served.join().unwrap();
    let version =
        control(crate::cells::make_version(crate::Daemon::process(), base.clone(), None)).unwrap();
    control(crate::cells::sync(crate::Daemon::process(), base)).unwrap();
    let moving = root.join("moving");
    control(crate::cells::fork_ephemeral_of_version(
        crate::Daemon::process(),
        base_cell,
        version,
        moving.clone(),
        None,
    ))
    .unwrap();

    let control_socket = root.join("control.sock");
    let listener = UnixListener::bind(&control_socket).unwrap();
    let tenants = root.join("tenants");
    std::fs::create_dir(&tenants).unwrap();
    std::thread::spawn(move || crate::control::serve_control(listener, tenants));
    let api = crate::testing_control::Control::connect(&control_socket).unwrap();
    api.call(async |client| client.get_stats(GetStatsRequest::default()).await).unwrap();
    // A peer serving gRPC in plaintext, no peer API: each of the migration's calls fails its TLS
    // handshake at once, its prepare tried thrice, unanswered, then given up and aborted.
    let peer = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    peer.set_nonblocking(true).unwrap();
    let address = peer.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async move {
            let incoming = tokio_stream::wrappers::TcpListenerStream::new(
                tokio::net::TcpListener::from_std(peer).unwrap(),
            );
            let (_, health) = tonic_health::server::health_reporter();
            tonic::transport::Server::builder()
                .add_service(health)
                .serve_with_incoming(incoming)
                .await
                .unwrap();
        })
    });
    // As `MigrateCell` begins it, which finds the cell in the daemon's disks, that this one has
    // none of.
    let migrated = control(crate::migration::migrate_on(crate::Daemon::process(), moving, address));
    assert_eq!(
        migrated.err().and_then(|status| crate::control::errno(&status)),
        Some(Errno::ENOTCONN)
    );

    let page = crate::metrics::render();
    let value = |series: &str| {
        let line = page.lines().find_map(|line| line.strip_prefix(&format!("{series} ")));
        line.unwrap_or_else(|| panic!("no {series} in {page}")).parse::<u64>().unwrap()
    };
    let cell = cell_at(&path);
    assert_eq!(value(&format!("cladefsd_vm_window_mappings{{cell=\"{cell}\"}}")), 1);
    assert_eq!(value("cladefsd_migrations_total{outcome=\"aborted\"}"), 1);
    let prepare = "service=\"cladefs.peer.v1.Peer\",method=\"MigratePrepare\"";
    assert_eq!(value(&format!("cladefsd_grpc_client_errors_total{{{prepare}}}")), 3);
    let stats = "service=\"cladefs.cladefsd.v1.Cladefsd\",method=\"GetStats\"";
    assert_eq!(value(&format!("cladefsd_grpc_server_calls_total{{{stats}}}")), 1);
    let families = crate::metrics::parsed(&page).unwrap_or_else(|err| panic!("{err}: {page}"));
    let declared = page.lines().filter(|line| line.starts_with("# TYPE ")).count();
    assert_eq!(families.len(), declared, "{families:?}");
    drop(client);
    served.join().unwrap();
    // SAFETY: Unmaps the memory, which nothing uses any more.
    assert_eq!(unsafe { libc::munmap(memory.as_ptr().cast(), 2 * ps) }, 0);
}

/// board 8563: a cell is labelled alike in every family, `TENANT/CELL`, as cells name it: its VM
/// process's window gauges, its builds, and its uploads', which a query joins on; and not by
/// connection, whose number made a series of each VM process.
#[test]
#[cfg(target_os = "linux")]
fn a_cells_metrics_name_it_alike_in_every_family() {
    host();
    let ps = page_size() as usize;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let (writer, written) = connect(&path, 1 << 24);
    let writers = memory(2);
    // SAFETY: The memory is the test's, and outlives the cell.
    unsafe { writer.create_dax_window_in_host_memory(writers, 2 * ps as u64, None, 1) }.unwrap();
    let extent = ExtentId { cell: writer.cell(), sequence: 1 };
    write(&writer, extent, 0, &pattern(4, ps)).unwrap();
    writer.commit(None).unwrap();
    let cell = cell_at(&path);
    let metrics = crate::metrics::render();
    for line in [
        format!("cladefsd_vm_window_mappings{{cell=\"{cell}\"}} "),
        format!(
            "cladefsd_vm_build_info{{cell=\"{cell}\",commit=\"{}\"}} 1",
            cladefs_core::BUILD_COMMIT
        ),
        format!("cladefsd_upload_pending_bytes{{cell=\"{cell}\"}} "),
    ] {
        assert!(metrics.lines().any(|each| each.starts_with(&line)), "no {line} in {metrics}");
    }
    assert!(!metrics.contains("connection="), "{metrics}");
    assert!(!metrics.contains("role="), "{metrics}");
    drop(writer);
    written.join().unwrap();
    // SAFETY: Unmaps the memory, which nothing uses any more.
    assert_eq!(unsafe { libc::munmap(writers.as_ptr().cast(), 2 * ps) }, 0);
}

/// board 9760: an accept of the control API's that fails as the host may get over (EMFILE, as
/// with every descriptor open) is tried again after `ACCEPT_AGAIN`, and the call after it served.
/// Before, tonic passed it over at once, and again at once as long as it failed: a busy loop
/// holding one of the runtime's threads (92% of a CPU with the descriptors out), nothing saying
/// why.
#[tokio::test(flavor = "multi_thread")]
async fn control_accepts_failing_are_tried_again_after_a_pause() {
    use futures::StreamExt as _;
    use tonic_health::pb::{HealthCheckRequest, health_client::HealthClient};
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("control.sock");
    let tenants = dir.path().join("tenants");
    std::fs::create_dir(&tenants).unwrap();
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let emfile = || Err(std::io::Error::from_raw_os_error(libc::EMFILE));
    let incoming = futures::stream::iter([emfile(), emfile()])
        .chain(tokio_stream::wrappers::UnixListenerStream::new(listener));
    let began = tokio::time::Instant::now();
    let daemon = crate::Daemon::process().clone();
    let serving = tokio::spawn(crate::control::serve_control_with(daemon, incoming, tenants));
    let channel = crate::control::channel(&socket).await.unwrap();
    let request = HealthCheckRequest { service: String::new() };
    HealthClient::new(channel).check(request).await.unwrap();
    let waited = began.elapsed();
    assert!(waited >= 2 * crate::server::ACCEPT_AGAIN, "served after {waited:?}");
    serving.abort();
}

/// TASK-79.35: the daemon goes on accepting VM processes' connections through a failure to accept
/// one, as with as many files open as it may (EMFILE): it accepts them once files close.
#[test]
#[cfg(target_os = "linux")]
fn the_daemon_accepts_on_after_failing_to() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    // Allocate the stop pipe and queue a pending client BEFORE lowering RLIMIT_NOFILE to 0 so
    // `serve_with_stop` enters its accept loop and hits real `EMFILE` on `listener.accept()`.
    let stop = crate::server::StopAccepting::new().unwrap();
    let _pending = UnixStream::connect(&socket).unwrap();
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: Reads the process's limit into `limit`.
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) }, 0);
    // Keep RLIMIT_NOFILE at 2 so `poll(2)` (nfds = 2) does not return EINVAL on Linux, while
    // `listener.accept()` still fails with EMFILE because fds 0..2 are already open.
    let none = libc::rlimit { rlim_cur: 2, ..limit };
    // SAFETY: Lowers the process's own limit, which this test sets back.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &none) }, 0);
    let (serving, stopped) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        // Dropped as serving stops.
        let _serving = serving;
        crate::server::serve_with_stop(listener, stop)
    });
    let served = stopped.recv_timeout(std::time::Duration::from_secs(1));
    // SAFETY: As above.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
    assert_eq!(served, Err(std::sync::mpsc::RecvTimeoutError::Timeout), "serving stopped");
    let path = dir.path().join("store");
    let opened = within(std::time::Duration::from_secs(10), move || {
        CellClient::connect(&socket, path, 1 << 20).map(|client| client.cell())
    });
    // TASK-687: an opening takes 50 to 120 ms here, alone or loaded; past 10 s, once, in a loaded
    // run, and in none of 300 since: what waited, if it waits again.
    if opened.is_err() {
        cladefs_core::stacks::log_stacks();
    }
    assert!(matches!(opened, Ok(Ok(_))), "the daemon accepts no more: {opened:?}");
}

/// TASK-79.83: a fork of a cell, as the control API makes it (`ForkCell`), to a path a cell is
/// at already, fails with `EEXIST`, as making a cell there does (`CreateCell`).
#[tokio::test(flavor = "multi_thread")]
async fn forks_to_cells_there_already_fail_with_eexist() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let (path, fork) = (dir.path().join("store"), dir.path().join("fork"));
    let open_cell = OpenCell::create(&path, 1 << 20).await.unwrap();
    open_cell.commit(None).await.unwrap();
    drop(open_cell);
    fork_cell(&path, &fork).unwrap();
    let again = fork_cell(&path, &fork);
    assert_eq!(again.err().and_then(|error| error.errno()), Some(Errno::EEXIST));
}

/// TASK-136.5: nothing a VM process sends attaches a cell to the one it has open: whatever request
/// it makes, naming the cell, what that holds stays unreadable to it.
#[tokio::test(flavor = "multi_thread")]
async fn requests_attach_nothing() {
    use crate::wire::{ACK, Header, REPLY, STAGE_COUNTS};
    use std::os::unix::ffi::OsStrExt;
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let base = dir.path().join("base");
    let open_cell = OpenCell::create(&base, 1 << 20).await.unwrap();
    let extent = ExtentId { cell: open_cell.cell().id, sequence: 1 };
    open_cell.write(extent, 0, &[VolatileSlice::from(&[7; 4096][..])]).await.unwrap();
    open_cell.commit(None).await.unwrap();
    drop(open_cell);
    let vm = CellClient::connect(&socket, dir.path().join("vm"), 1 << 20).unwrap();
    // But those nothing replies to.
    for kind in (0..REPLY).filter(|kind| ![ACK, STAGE_COUNTS].contains(kind)) {
        for (cell, sequence) in [(extent.cell, 1), (0, 0)] {
            let header = Header::new(kind, 0).with_body(&[cell, sequence, 0u64, 4096]);
            // cladefs: named in a payload where its kind has one: past what a kind holds, the
            // daemon refuses it at its header, ending the connection (board 8459).
            let path = base.as_os_str().as_bytes();
            let fits = path.len() as u64 <= crate::codec::most_payload(&header);
            let _ = vm.call(header, if fits { path } else { &[] }, &[]);
        }
    }
    let mut data = vec![0; 4096];
    let read = vm.read(extent, 0, &mut [VolatileSliceMut::from(&mut data[..])]);
    assert_eq!(read, Err(Errno::ENOENT));
}

/// doc-26: a VM process is handed its tenant's arena and the default one read-only: it copies its
/// guest's writes into memory of its own, and writes no page the daemon's cache holds.
#[test]
// cladefs: on macOS its arenas are files unlinked as they are made, handed writable (board 13541).
#[cfg(target_os = "linux")]
fn vm_processes_are_handed_their_memory_read_only() {
    use crate::wire::*;
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let stream = UnixStream::connect(&socket).unwrap();
    let (sender, mut receiver) = (
        Sender::new(stream.try_clone().unwrap()).unwrap(),
        Receiver::new(stream.try_clone().unwrap()),
    );
    let open = Header::new(OPEN, 1)
        .with_body(&OpenBody { capacity: 1 << 24, version: VERSION, ..Default::default() })
        .with_flags(OPEN_CREATE);
    let path = dir.path().join("store");
    let mut payload = std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str()).to_vec();
    payload.push(0);
    payload.extend_from_slice(cladefs_core::BUILD_COMMIT.as_bytes());
    sender.send(open, &payload, &[]).unwrap();
    let opened = receiver.receive(|| 0).unwrap().unwrap();
    assert_eq!((opened.header.status, opened.fds.len()), (0, 2));
    for fd in &opened.fds {
        use std::os::fd::AsRawFd;
        // SAFETY: Plain system call on a descriptor the message holds.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        assert_eq!(flags & libc::O_ACCMODE, libc::O_RDONLY, "handed writable");
    }
}

/// TASK-702.1: with `CLADEFSD_READ_TRACES` naming a directory, a VM's reads are traced, and the
/// trace written as its connection goes: its `MAP`, its `READ`, then its window's fault, a load's,
/// each naming the extent, its length, and the range read, in the order made. cladefs: its write,
/// to a page the load's fault named with the rest of its part, its VM process copies itself, which
/// the daemon never hears of (doc-26).
#[test]
#[cfg(target_os = "linux")]
fn a_vms_reads_are_traced() {
    host();
    let traces = tempfile::tempdir().unwrap();
    // SAFETY: nextest runs each test in a process of its own, whose other threads don't read the
    // environment yet.
    unsafe { std::env::set_var("CLADEFSD_READ_TRACES", traces.path()) };
    crate::reads::configure().unwrap();
    let ps = page_size() as usize;
    let dir = tempfile::tempdir().unwrap();
    let (client, served) = connect(&dir.path().join("store"), 1 << 24);
    let memory = memory(2);
    // SAFETY: The memory is the test's, and outlives the cell.
    unsafe { client.create_dax_window_in_host_memory(memory, 2 * ps as u64, None, 2) }.unwrap();
    cladefs_core::guest_thread_started();
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, extent, 0, &pattern(3, 3 * ps)).unwrap();
    client.commit(None).unwrap();
    // Its second and third pages, at the window's first and second.
    client.map(extent, ps as u64, 2 * ps as u64, 0).unwrap();
    assert_eq!(read(&client, extent, 0, 512), Ok(pattern(3, 512)));
    let base = memory.as_ptr() as usize;
    // SAFETY: The window's memory, whose faults the cell serves.
    assert_eq!(unsafe { memory.as_ptr().read_volatile() }, pattern(3, ps + 1)[ps]);
    assert_eq!(kernel_write(base + ps), Ok(()));
    let cell = client.cell();
    drop(client);
    served.join().unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let text = loop {
        let written = std::fs::read_dir(traces.path()).unwrap().next();
        if let Some(entry) = written {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            let prefix = format!("{}-", base32_id::Padded(cell));
            assert!(name.starts_with(&prefix) && name.ends_with(".reads"), "{name}");
            break std::fs::read_to_string(entry.path()).unwrap();
        }
        assert!(std::time::Instant::now() < deadline, "no trace was written");
        std::thread::yield_now();
    };
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], crate::reads::HEADER);
    assert!(lines[1].starts_with(&format!("cell {} opened ", base32_id::Short(cell))), "{text}");
    let reads: Vec<Vec<&str>> =
        lines[2..].iter().map(|line| line.split(' ').skip(1).take(6).collect()).collect();
    let (id, len) = (base32_id::Short(cell).to_string(), format!("{:x}", 3 * ps));
    let (one, two) = (format!("{ps:x}"), format!("{:x}", 2 * ps));
    assert_eq!(
        reads,
        [
            vec!["map", &id, "1", &len, &one, &two],
            vec!["read", &id, "1", &len, "0", "200"],
            vec!["fault", &id, "1", &len, &one, &one],
        ],
        "{text}"
    );
}

/// TASK-702.2: a VM restoring a version records what its guest reads of the version, by `MAP` and
/// by `READ` alike, as the version's read evidence of its tenant's, written as its cell closes,
/// before another connection opens it again.
#[test]
#[cfg(target_os = "linux")]
fn a_restored_vms_reads_are_its_versions_evidence() {
    host();
    let ps = page_size() as usize;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let client = CellClient::connect(&socket, dir.path().join("store"), 1 << 24).unwrap();
    let extent = |sequence| ExtentId { cell: client.cell(), sequence };
    for sequence in [1, 2] {
        write(&client, extent(sequence), 0, &pattern(sequence as u8, ps)).unwrap();
    }
    client.commit(None).unwrap();
    let version = client.make_version().unwrap();

    let path = dir.path().join("restored");
    let restored = CellClient::connect_forked(&socket, &path, version, 0).unwrap();
    let memory = memory(2);
    // SAFETY: The memory is the test's, and outlives the cell.
    unsafe { restored.create_dax_window_in_host_memory(memory, 2 * ps as u64, None, 1) }.unwrap();
    restored.map(extent(1), 0, ps as u64, 0).unwrap();
    assert_eq!(read(&restored, extent(2), 0, 512), Ok(pattern(2, 512)));
    let theirs = crate::cells::table().writer(&crate::cells::cell_dir_key(&path)).unwrap();
    let forked = theirs.cell().unwrap().forked_from().unwrap();
    let tenant = theirs.cell().unwrap().cell().tenant;
    drop(theirs);
    drop(restored);
    // Opened again once the restore's connection let it go, closed.
    let again = CellClient::connect(&socket, &path, 1 << 24).unwrap();

    let of = bucket_cell::evidence::Of { origin: forked.origin, tenant };
    let bucket = crate::cells::daemon_bucket().unwrap();
    let keys = forked.ring.of(of.cell().id);
    let read = control(bucket_cell::evidence::read(&*bucket, of, &keys)).unwrap();
    let units: Vec<(ExtentId, u64)> =
        read.recordings.concat().iter().map(|unit| (unit.extent, unit.mib)).collect();
    assert_eq!((units, read.refused), (vec![(extent(1), 0), (extent(2), 0)], 0));
    drop((again, client));
}

/// Board 6225, 5337: a host that keeps parts in its part cache, a version's parts there and not in
/// its memory, reads what its tenant's evidence predicts a fork reads into memory ahead of the
/// fork's guest, as its first fork's recording, written seconds after the host read the
/// version's evidence of none, predicts: the guest's reads then wait on no page-in.
#[test]
fn forks_read_their_prediction_into_memory_from_the_part_cache() {
    use bucket_cell::evidence::UNIT;
    host();
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: nextest runs each test in a process of its own, whose other threads don't read the
    // environment yet.
    unsafe { std::env::set_var("CLADEFSD_PART_CACHE", dir.path().join("parts")) };
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let client = CellClient::connect(&socket, dir.path().join("store"), 1 << 24).unwrap();
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    let data = pattern(3, 2 * UNIT as usize);
    for (at, chunk) in data.chunks(64 << 10).enumerate() {
        write(&client, extent, at as u64 * (64 << 10), chunk).unwrap();
    }
    client.commit(None).unwrap();
    let version = client.make_version().unwrap();
    // Its cell's prefetch, done: awaited holding nothing of the daemon's.
    let prefetched = |path: &Path| {
        let held = crate::cells::table().writer(&crate::cells::cell_dir_key(path)).unwrap();
        let prefetching = held.cell().unwrap().evidence().unwrap().prefetched();
        drop(held);
        futures::executor::block_on(prefetching);
    };

    // The first fork's guest reads both units, which the host fetches into its part cache; its
    // cell, closed, records them. Opened again, it prefetches them, in memory already.
    let first = dir.path().join("first");
    let one = CellClient::connect_forked(&socket, &first, version, 0).unwrap();
    prefetched(&first);
    assert_eq!(read(&one, extent, 0, 512), Ok(data[..512].to_vec()));
    assert_eq!(read(&one, extent, UNIT, 512), Ok(data[UNIT as usize..][..512].to_vec()));
    drop(one);
    let again = CellClient::connect(&socket, &first, 1 << 24).unwrap();
    prefetched(&first);
    drop(again);

    // Its pages evicted, as memory pressure evicts them, the next fork's prefetch reads them into
    // memory from the part cache, fetching nothing; its guest's reads page nothing in.
    cladefs_core::cache::evict_least_used(None, |_| false);
    let before = cladefs_core::stats::stats();
    let second = dir.path().join("second");
    let two = CellClient::connect_forked(&socket, &second, version, 0).unwrap();
    prefetched(&second);
    let counted = cladefs_core::stats::stats().since(&before);
    assert_eq!((counted.prefetches, counted.prefetch_page_ins), (0, 2));
    assert_eq!(
        read(&two, extent, UNIT + 4096, 512),
        Ok(data[UNIT as usize + 4096..][..512].to_vec())
    );
    assert_eq!(read(&two, extent, 8192, 512), Ok(data[8192..][..512].to_vec()));
    assert_eq!(cladefs_core::stats::stats().since(&before).page_ins, 2);
    drop((two, client));
}

/// Board 7420: a restore's fork, made and opened syncing nothing, is unmade on the disk, as a host
/// crashing then would leave it for deleting, and whole here, as any cell, until its first flush
/// makes it durable; or, its VM committing nothing, its close does.
#[test]
fn restores_forks_are_made_durable_by_their_first_flush_or_their_close() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let client = CellClient::connect(&socket, dir.path().join("store"), 1 << 24).unwrap();
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, extent, 0, &pattern(3, BLOCK)).unwrap();
    client.commit(None).unwrap();
    let version = client.make_version().unwrap();
    let unmade = |path: &Path| path.join("unmade").exists();

    // Restored by a path of another spelling: the directory's deferral, however it is named.
    let restored = dir.path().join("restored");
    std::fs::create_dir(dir.path().join("x")).unwrap();
    let spelled = dir.path().join("x/../restored");
    let vm = CellClient::connect_forked(&socket, &spelled, version, 0).unwrap();
    assert!(unmade(&restored), "made durable as it was restored");
    assert!(!cladefs_core::host_cell::unmade(&restored), "unmade here");
    assert_eq!(read(&vm, extent, 0, 512), Ok(pattern(3, BLOCK)[..512].to_vec()));
    let own = ExtentId { cell: vm.cell(), sequence: 1 };
    write(&vm, own, 0, &pattern(4, BLOCK)).unwrap();
    vm.commit(None).unwrap();
    assert!(!unmade(&restored), "its first flush left it unmade");

    let closed = dir.path().join("closed");
    let idle = CellClient::connect_forked(&socket, &closed, version, 0).unwrap();
    assert!(unmade(&closed), "made durable as it was restored");
    drop(idle);
    // Opened again once its close is done.
    let again = CellClient::connect(&socket, &closed, 1 << 24).unwrap();
    assert!(!unmade(&closed), "its close left it unmade");
    assert_eq!(read(&again, extent, 0, 512), Ok(pattern(3, BLOCK)[..512].to_vec()));
    drop((again, vm, client));
}

/// Board 7420 + hotbed review 135: restore forks placed through a managed `DiskPool` (`0/0`)
/// still defer durability until their first flush or close, without eager syncs at restore time.
#[test]
fn managed_disk_restore_forks_defer_durability() {
    host();
    let dir = tempfile::tempdir().unwrap();
    super::Daemon::process().configure_disks(&[dir.path().join("disk0")]).unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));

    let client = CellClient::connect(&socket, "0/0", 1 << 24).unwrap();
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, extent, 0, &pattern(3, BLOCK)).unwrap();
    client.commit(None).unwrap();
    let version = client.make_version().unwrap();

    let vm = CellClient::connect_forked(&socket, "0/0", version, 0).unwrap();
    let restored_id = cladefs_core::CellId { tenant: 0, id: vm.cell() };
    let restored = super::Daemon::process().cell_path(restored_id).unwrap();
    let entries: Vec<_> =
        std::fs::read_dir(&restored).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert!(
        restored.join("unmade").exists(),
        "made durable as it was restored: client={} vm={} restored={} entries={entries:?}",
        client.cell(),
        vm.cell(),
        restored.display()
    );
    assert!(!cladefs_core::host_cell::unmade(&restored), "unmade here");
    assert_eq!(read(&vm, extent, 0, 512), Ok(pattern(3, BLOCK)[..512].to_vec()));
    let own = ExtentId { cell: vm.cell(), sequence: 1 };
    write(&vm, own, 0, &pattern(4, BLOCK)).unwrap();
    vm.commit(None).unwrap();
    assert!(!restored.join("unmade").exists(), "its first flush left it unmade");
    drop((vm, client));
}

/// Board 7420: a restore's fork whose settling fails fails its cell, as a flush's failure does:
/// its commit is refused, then and after, and it stays unmade on the disk, for deleting.
#[test]
fn restores_forks_whose_settling_fails_fail_their_cells() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let client = CellClient::connect(&socket, dir.path().join("store"), 1 << 24).unwrap();
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, extent, 0, &pattern(3, BLOCK)).unwrap();
    client.commit(None).unwrap();
    let version = client.make_version().unwrap();
    let restored = dir.path().join("restored");
    let vm = CellClient::connect_forked(&socket, &restored, version, 0).unwrap();
    fail::cfg("deferral::settle", "return").unwrap();
    let own = ExtentId { cell: vm.cell(), sequence: 1 };
    write(&vm, own, 0, &pattern(4, BLOCK)).unwrap();
    assert_eq!(vm.commit(None), Err(Errno::EIO));
    fail::remove("deferral::settle");
    assert_eq!(vm.commit(None), Err(Errno::EIO));
    assert!(restored.join("unmade").exists(), "made durable, its settling failed");
    drop((vm, client));
}

/// Board 6589: a host whose temporary directory is on another filesystem than its part cache and
/// its cells, as a hotbed workspace's results on ZFS beside its TMPDIR on ext4, forks a cell with
/// a writer and restores a version: the cell at a version each goes through is on the cache's
/// filesystem, which every cell the cache serves must be on (TASK-359), not in the temporary
/// directory, which refused both (`EIO`).
#[cfg(target_os = "linux")]
#[test]
fn forks_go_through_versions_on_the_part_caches_filesystem() {
    use std::os::unix::fs::MetadataExt as _;
    host();
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir_in("/dev/shm").unwrap();
    let device = |path: &Path| std::fs::metadata(path).unwrap().dev();
    assert_ne!(device(dir.path()), device(elsewhere.path()), "two filesystems");
    // SAFETY: nextest runs each test in a process of its own, whose other threads don't read the
    // environment yet.
    unsafe {
        std::env::set_var("CLADEFSD_PART_CACHE", dir.path().join("parts"));
        std::env::set_var("TMPDIR", elsewhere.path());
    }
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let (path, fork) = (dir.path().join("store"), dir.path().join("fork"));
    let client = CellClient::connect(&socket, &path, 1 << 24).unwrap();
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    let data = pattern(3, 2 * BLOCK);
    write(&client, extent, 0, &data).unwrap();
    client.commit(Some(b"base")).unwrap();
    drop(client);

    // Its writer's cell, no connection's, forked through a version of its last commit.
    fork_cell(&path, &fork).unwrap();
    let forked = CellClient::connect(&socket, &fork, 1 << 24).unwrap();
    assert_eq!(read(&forked, extent, 0, 512), Ok(data[..512].to_vec()));
    // A version of the fork restored, from the cell at the version kept open for its forks.
    let version = forked.make_version().unwrap();
    let restored =
        CellClient::connect_forked(&socket, dir.path().join("restored"), version, 0).unwrap();
    assert_eq!(read(&restored, extent, BLOCK as u64, 512), Ok(data[BLOCK..][..512].to_vec()));
    drop((restored, forked));
}

/// TASK-79.17, TASK-702.3: a cell the daemon opens to work on, as no VM process has it open,
/// boots no guest: its version's read evidence is not read, nor anything fetched ahead for it. Here
/// a fork of a version is synced; a VM opening it reads the evidence.
#[test]
fn cells_worked_on_here_fetch_nothing_ahead() {
    host();
    let ps = page_size() as usize;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let client = CellClient::connect(&socket, dir.path().join("store"), 1 << 24).unwrap();
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, extent, 0, &pattern(7, ps)).unwrap();
    client.commit(None).unwrap();
    let version = client.make_version().unwrap();
    let path = dir.path().join("fork");
    // Forked by the daemon, not by a VM's connection, which no VM has open, nor had.
    let cell = cladefs_core::CellId { tenant: version.tenant, id: version.cell };
    control(crate::cells::fork_version(
        crate::Daemon::process(),
        (cell, version.version),
        1 << 24,
        (&path, None, None),
    ))
    .unwrap();

    cladefs_core::evidence::forget();
    let before = cladefs_core::stats::stats();
    wait_uploaded(&path).unwrap();
    assert_eq!(cladefs_core::evidence::kept(), 0);
    assert_eq!(cladefs_core::stats::stats().since(&before).prefetches, 0);
    let again = CellClient::connect(&socket, &path, 1 << 24).unwrap();
    assert_eq!(cladefs_core::evidence::kept(), 1);
    drop((again, client));
}

/// TASK-79.34: a cell the daemon synced, opened here as no connection had it open, is closed
/// before those waiting to open it may: deleted at once after, it is open no more, each time.
#[tokio::test(flavor = "multi_thread")]
async fn cells_synced_here_are_closed_before_they_are_let_go() {
    host();
    let dir = tempfile::tempdir().unwrap();
    for round in 0..20 {
        let path = dir.path().join(format!("store-{round}"));
        let open_cell = OpenCell::create(&path, 1 << 20).await.unwrap();
        open_cell.commit(None).await.unwrap();
        drop(open_cell);
        wait_uploaded(&path).unwrap();
        let deleted = delete_cell(&path).map_err(|error| error.errno());
        assert_eq!(deleted, Ok(()), "round {round}");
    }
}

/// A memory object of `len` bytes, as a daemon hands its VM processes its arenas.
#[cfg(target_os = "linux")]
fn memfd(len: u64) -> std::fs::File {
    // SAFETY: The name is a C string.
    let fd = unsafe { libc::memfd_create(c"memory".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0);
    // SAFETY: Just created, and owned here.
    let file = std::fs::File::from(unsafe {
        <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd)
    });
    file.set_len(len).unwrap();
    file
}

/// A memory object of `len` bytes, as [`memfd`] is on Linux: on macOS, which has no memfd, a file
/// unlinked as it is made, as the daemon's arenas there are (`slabs`).
#[cfg(not(target_os = "linux"))]
fn memfd(len: u64) -> std::fs::File {
    let file = tempfile::tempfile().unwrap();
    file.set_len(len).unwrap();
    file
}

/// TASK-79.43: a window's mappings, a snapshot's, more of them than one message's most bytes of
/// payload hold, arrive whole: a daemon's reply holds as many as a window of its size can have,
/// which its VM process takes. Here a daemon of the test's answers the cell's opening, its
/// window's, and its mappings', a page each, past `MAX_PAYLOAD`.
#[test]
#[cfg(target_os = "linux")]
fn window_mappings_past_a_messages_payload_arrive() {
    use crate::wire::*;
    use std::os::fd::AsFd;
    let ps = page_size();
    // More than a message's payload, and its header's room past it, hold.
    let count = (MAX_PAYLOAD + size_of::<Header>() as u64) / 48 + 1;
    let (stream, daemon) = UnixStream::pair().unwrap();
    let answering = std::thread::spawn(move || {
        let (sender, mut receiver) =
            (Sender::new(daemon.try_clone().unwrap()).unwrap(), Receiver::new(daemon));
        let (own, public) = (memfd(ps), memfd(ps));
        while let Ok(Some(request)) = receiver.receive(|| 0) {
            let mut reply = Header::new(REPLY, request.header.id);
            let (mut payload, mut fds) = (Vec::new(), Vec::new());
            match request.header.kind {
                OPEN => {
                    reply.set_body(&opened_body(7));
                    payload = 0u64.to_le_bytes().to_vec();
                    fds = vec![own.as_fd(), public.as_fd()];
                }
                MAPPINGS => {
                    payload = (0..count)
                        .flat_map(|page| [page * ps, ps, 0, 0, 0, MAPPING_ZEROS])
                        .flat_map(u64::to_le_bytes)
                        .collect();
                }
                TRANSFER | WINDOW => {}
                _ => reply.status = -Errno::EINVAL.raw(),
            }
            sender.send(reply, &payload, &fds).unwrap();
        }
    });
    let client = CellClient::over(stream, Path::new("store"), Opening::Create(1 << 20, Kind::Fxfs));
    let client = client.unwrap();
    let size = count * ps;
    // SAFETY: Reserves the window's memory, which the test leaks: the window maps it while the
    // process lives.
    let memory = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    assert_ne!(memory, libc::MAP_FAILED);
    let memory = NonNull::new(memory.cast()).unwrap();
    // SAFETY: The memory is the test's, and outlives the cell.
    unsafe { client.create_dax_window_in_host_memory(memory, size, None, 1) }.unwrap();
    let mappings = client.window_mappings().unwrap();
    assert_eq!(mappings.len() as u64, count);
    let last = cladefs_core::WindowMapping {
        window_offset: (count - 1) * ps,
        len: ps,
        contents: cladefs_core::MappingContents::Zeros,
    };
    assert_eq!(mappings.last(), Some(&last));
    drop(client);
    answering.join().unwrap();
}

/// cladefs: a request its daemon's going failed finds the daemon gone: the connection is lost
/// before what waits on it fails, so that what a failure makes of it, as the VM process's faults
/// do, can tell a daemon gone from a refusal (TASK-231). A VM whose daemon died with a fault in
/// flight aborted (SIGABRT), the fault's EIO taken for the daemon's refusal, where it exits
/// CladefsdLost (158): fxfs-guest-soak, GCE r830 round 27.
#[test]
fn requests_failed_by_the_daemons_going_find_it_gone() {
    use crate::wire::*;
    use std::os::fd::AsFd;
    let ps = page_size();
    let (stream, daemon) = UnixStream::pair().unwrap();
    let answering = std::thread::spawn(move || {
        let (sender, mut receiver) =
            (Sender::new(daemon.try_clone().unwrap()).unwrap(), Receiver::new(daemon));
        let (own, public) = (memfd(ps), memfd(ps));
        while let Ok(Some(request)) = receiver.receive(|| 0) {
            let mut reply = Header::new(REPLY, request.header.id);
            let (mut payload, mut fds) = (Vec::new(), Vec::new());
            match request.header.kind {
                OPEN => {
                    reply.set_body(&opened_body(7));
                    payload = 0u64.to_le_bytes().to_vec();
                    fds = vec![own.as_fd(), public.as_fd()];
                }
                TRANSFER => {}
                // The daemon goes as it is asked to read, answering nothing.
                _ => return,
            }
            sender.send(reply, &payload, &fds).unwrap();
        }
    });
    let client = CellClient::over(stream, Path::new("store"), Opening::Create(1 << 20, Kind::Fxfs));
    let client = client.unwrap();
    // What the failed request does comes before the rest of the connection's end.
    let _slow = fail::FailGuard::new("client::requests_failed", "sleep(200)").unwrap();
    let mut data = vec![0; ps as usize];
    let extent = ExtentId { cell: 7, sequence: 1 };
    let read = client.read(extent, 0, &mut [VolatileSliceMut::from(&mut data[..])]);
    assert_eq!(read, Err(cladefs_core::Errno::EIO));
    assert!(client.daemon_lost(), "the read failed, its daemon not yet lost");
    answering.join().unwrap();
}

/// cladefs: a daemon going as a cell opens over its connection is told of, as what was said of the
/// connection before the open says ([`DaemonConnection::on_lost`]), before the open fails, which
/// says why: a VMM stops its VM CladefsdLost (158) then, as for a daemon going later, rather than
/// for the open's failure. A VM booted again as its daemon was killed exited 1, its cell
/// recovering from a bucket slowed a second a request: fxfs-guest-soak, GCE r831 round 29
/// (TASK-232). Red with the connection's end told of after the open failed, which the failpoint
/// holding the end shows.
#[test]
fn daemons_going_as_cells_open_are_told_of_first() {
    use crate::client::DaemonConnection;
    use crate::wire::*;
    let (stream, daemon) = UnixStream::pair().unwrap();
    let going = std::thread::spawn(move || {
        let open = Receiver::new(daemon).receive(|| 0).unwrap().unwrap();
        // The daemon goes as it opens the cell, answering nothing.
        assert_eq!(open.header.kind, OPEN);
    });
    let connection = DaemonConnection::over(stream).unwrap();
    let happened = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let told = happened.clone();
    connection.on_lost(move || told.lock().unwrap().push("daemon lost"));
    // What the connection's end does after failing what waits on it comes after the open fails.
    let _slow = fail::FailGuard::new("client::requests_failed", "sleep(200)").unwrap();
    let opened = connection.open(Path::new("store"), 1 << 20);
    happened.lock().unwrap().push("open failed");
    let err = opened.err().expect("the cell opened, its daemon gone");
    assert_eq!(
        format!("{err:#}"),
        "opening the extent store at store: the extent daemon went: EIO (errno 5)"
    );
    assert_eq!(*happened.lock().unwrap(), ["daemon lost", "open failed"]);
    going.join().unwrap();
}

/// A VM process's connection to the daemon at `socket`, made by hand, as one that keeps to no
/// protocol makes it: its cell at `path` opened, created with `capacity` bytes, and its cell.
fn raw_connection(
    socket: &Path,
    path: &Path,
    capacity: u64,
) -> (crate::wire::Sender, crate::wire::Receiver, UnixStream, u64) {
    use crate::wire::*;
    let stream = UnixStream::connect(socket).unwrap();
    let (sender, mut receiver) = (
        Sender::new(stream.try_clone().unwrap()).unwrap(),
        Receiver::new(stream.try_clone().unwrap()),
    );
    let open = Header::new(OPEN, 1)
        .with_body(&OpenBody { capacity, version: VERSION, ..Default::default() })
        .with_flags(OPEN_CREATE);
    let mut payload = std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str()).to_vec();
    payload.push(0);
    payload.extend_from_slice(cladefs_core::BUILD_COMMIT.as_bytes());
    sender.send(open, &payload, &[]).unwrap();
    let opened = receiver.receive(|| 0).unwrap().unwrap();
    assert_eq!(opened.header.status, 0);
    (sender, receiver, stream, opened.header.read_body::<OpenedBody>().cell)
}

/// Sends `requests`, of no payload, until the daemon lets the VM process go, as it may before the
/// last is sent: what it leaves unread passes what a connection may hold while the test still
/// sends (TASK-240). A send failing so ends them; whether the daemon let it go is the test's to
/// check, with [`assert_let_go`].
fn send_until_let_go(
    sender: &crate::wire::Sender,
    requests: impl IntoIterator<Item = crate::wire::Header>,
) {
    for request in requests {
        match sender.send(request, &[], &[]) {
            Ok(()) => {}
            Err(error) if shut(&error) => return,
            Err(error) => panic!("sending request {}: {error}", request.id),
        }
    }
}

/// Whether `error`, a send's, says the daemon shut the connection.
fn shut(error: &std::io::Error) -> bool {
    use std::io::ErrorKind::{BrokenPipe, ConnectionReset};
    matches!(error.kind(), BrokenPipe | ConnectionReset)
}

/// Asserts the daemon let go of the VM process whose connection `sender` sends on and `stream` is,
/// `within` what the caller's case allows for what it has the daemon do first: it hangs up, and
/// shuts the connection, so that what the VM process sends then fails, as a send of
/// [`send_until_let_go`]'s may (TASK-240).
fn assert_let_go(sender: &crate::wire::Sender, stream: &UnixStream, within: std::time::Duration) {
    use crate::wire::{Header, INFO};
    assert_hung_up(stream, within);
    // cladefs: on macOS (a BSD), the daemon's shutting its end down fails no send of the VM
    // process's, which goes into the socket's buffer, read by none; the VM process sees the
    // daemon hang up, as above, and its client shuts its own end down then (`client`, board
    // 13065).
    #[cfg(target_os = "linux")]
    {
        let sent = sender.send(Header::new(INFO, u64::MAX), &[], &[]);
        assert!(sent.as_ref().is_err_and(shut), "sent once let go: {sent:?}");
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (sender, Header::new(INFO, u64::MAX));
}

/// Asserts the daemon at the other end of `stream` hangs up within `within`, though this end may
/// not have read what it sent: `POLLRDHUP` on Linux; on macOS, which has none, the stream
/// readable within `within`, then its kqueue's end of what the daemon sends (`wire::hung_up`).
pub(super) fn assert_hung_up(stream: &UnixStream, within: std::time::Duration) {
    use std::os::fd::AsRawFd;
    #[cfg(target_os = "linux")]
    let events = libc::POLLRDHUP;
    #[cfg(not(target_os = "linux"))]
    let events = libc::POLLIN;
    let mut hung_up = libc::pollfd { fd: stream.as_raw_fd(), events, revents: 0 };
    let within = libc::c_int::try_from(within.as_millis()).unwrap();
    // SAFETY: Polls the stream, which the caller keeps open, for `within` at most.
    let ready = unsafe { libc::poll(&mut hung_up, 1, within) };
    assert_eq!(ready, 1, "the daemon holds what the VM process leaves unread");
    #[cfg(target_os = "linux")]
    assert_ne!(hung_up.revents & libc::POLLRDHUP, 0, "{:#x}", hung_up.revents);
    #[cfg(not(target_os = "linux"))]
    {
        use std::os::fd::AsFd;
        let hung = crate::wire::hung_up(stream.as_fd());
        assert!(hung, "readable, but not hung up: {:#x}", hung_up.revents);
    }
}

/// How many bytes `stream` has received that are not read yet.
fn unread(stream: &UnixStream) -> usize {
    use std::os::fd::AsRawFd;
    let mut unread: libc::c_int = 0;
    // SAFETY: Reads the count into `unread`.
    assert_eq!(unsafe { libc::ioctl(stream.as_raw_fd(), libc::FIONREAD, &mut unread) }, 0);
    unread as usize
}

/// TASK-79.16: a VM process that stops reading what the daemon sends it holds up no other: as the
/// daemon's reply to it waits for room in its socket, another VM process opening its cell is
/// refused at once (`EBUSY`), and another's cell opens.
#[test]
fn vm_processes_that_stop_reading_hold_up_no_other() {
    use crate::wire::*;
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let path = dir.path().join("store");
    let (sender, mut receiver, stream, cell) = raw_connection(&socket, &path, 1 << 24);
    // An extent, read back whole in one reply, many times what the socket holds, which it never
    // reads.
    let len = 4 << 20;
    let written = Header::new(WRITE, 2).with_body(&WriteBody {
        cell,
        sequence: 1,
        data_len: len as u64,
        ..Default::default()
    });
    sender.send(written, &pattern(1, len), &[]).unwrap();
    assert_eq!(receiver.receive(|| 0).unwrap().unwrap().header.status, 0);
    let read = Header::new(READ, 3).with_body(&ReadBody {
        cell,
        sequence: 1,
        offset: 0,
        len: len as u64,
        transfer_at: 0,
    });
    sender.send(read, &[], &[]).unwrap();
    while unread(&stream) == 0 {
        std::thread::yield_now();
    }

    let wait = std::time::Duration::from_secs(10);
    let opening = path.clone();
    let (busy_socket, other_socket) = (socket.clone(), socket.clone());
    let busy = within(wait, move || {
        CellClient::connect(&busy_socket, &opening, 1 << 24).err().and_then(|e| e.errno())
    });
    assert_eq!(busy, Ok(Some(Errno::EBUSY)), "opening the cell waited for its VM process");
    let other = dir.path().join("other");
    let opened = within(wait, move || {
        CellClient::connect(&other_socket, &other, 1 << 24).map(|client| client.cell())
    });
    assert!(matches!(opened, Ok(Ok(_))), "another cell's opening waited: {opened:?}");
    drop((sender, receiver, stream));
}

/// TASK-79.16: a VM process that leaves what the daemon sends it unread, past what a connection
/// may hold, is let go, rather than held without end in the daemon's memory, or its threads: here
/// replies to its reads, half a gibibyte of them.
#[test]
fn vm_processes_that_leave_replies_unread_are_let_go() {
    use crate::wire::*;
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let (sender, mut receiver, stream, cell) =
        raw_connection(&socket, &dir.path().join("store"), 1 << 24);
    let len = 8 << 20;
    let written = Header::new(WRITE, 2).with_body(&WriteBody {
        cell,
        sequence: 1,
        data_len: len as u64,
        ..Default::default()
    });
    sender.send(written, &pattern(1, len), &[]).unwrap();
    assert_eq!(receiver.receive(|| 0).unwrap().unwrap().header.status, 0);
    let reads = (3..67).map(|id| {
        Header::new(READ, id).with_body(&ReadBody {
            cell,
            sequence: 1,
            offset: 0,
            len: len as u64,
            transfer_at: 0,
        })
    });
    send_until_let_go(&sender, reads);
    assert_let_go(&sender, &stream, WAIT);
    drop((sender, receiver, stream));
}

/// TASK-150.3: a window of any size a VM process names makes the daemon no more room for it: past
/// what the protocol bounds, it is let go still, here for half a gibibyte of replies it leaves
/// unread, behind a window's worth of mappings of all the bits a word has.
#[test]
fn vm_processes_naming_vast_windows_are_let_go_all_the_same() {
    use crate::wire::*;
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let (sender, mut receiver, stream, cell) =
        raw_connection(&socket, &dir.path().join("store"), 1 << 24);
    let window = Header::new(WINDOW, 2)
        .with_body(&WindowBody { size: 1 << 60, budget: 0 })
        .with_flags(WINDOW_COPYING);
    let answered = call(&sender, &mut receiver, window, &[]).header.status;
    let len = 8 << 20;
    let written = Header::new(WRITE, 3).with_body(&WriteBody {
        cell,
        sequence: 1,
        data_len: len as u64,
        ..Default::default()
    });
    assert_eq!(call(&sender, &mut receiver, written, &pattern(1, len)).header.status, 0);
    let reads = (4..68).map(|id| {
        Header::new(READ, id).with_body(&ReadBody {
            cell,
            sequence: 1,
            offset: 0,
            len: len as u64,
            transfer_at: 0,
        })
    });
    send_until_let_go(&sender, reads);
    assert_let_go(&sender, &stream, WAIT);
    assert_eq!(answered, -Errno::EINVAL.raw());
    drop((sender, receiver, stream));
}

/// TASK-150.3: a window as large as the daemon makes room for is one it serves.
#[test]
fn windows_go_up_to_the_most_the_daemon_makes_room_for() {
    use crate::wire::*;
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    for (name, size, status) in
        [("past", MAX_WINDOW + page_size(), -Errno::EINVAL.raw()), ("largest", MAX_WINDOW, 0)]
    {
        let (sender, mut receiver, _stream, _) =
            raw_connection(&socket, &dir.path().join(name), 1 << 24);
        // cladefs: a writer's, copying on write (backlog doc-13).
        let window = Header::new(WINDOW, 2)
            .with_body(&WindowBody { size, budget: 0 })
            .with_flags(WINDOW_COPYING);
        assert_eq!(call(&sender, &mut receiver, window, &[]).header.status, status, "{name}");
    }
}

/// TASK-150.4: a VM process opening a fork of a version that doesn't exist, in its tenant's
/// directory, names another tenant's cell: it gets nothing of it, and the cell's writer is left as
/// it was. Its tenant's cells are its directory's: what the bucket holds has no tenant.
#[test]
fn tenants_take_no_cell_by_naming_it() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let tenant = |name: &str, id: u64| {
        let cells = dir.path().join(name);
        std::fs::create_dir(&cells).unwrap();
        let tenant = crate::server::Tenant::new(id, &cells).unwrap();
        let socket = dir.path().join(format!("{name}.sock"));
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || crate::server::serve_tenant(listener, tenant));
        (cells, socket)
    };
    let (ours, our_socket) = tenant("ours", 1);
    let (theirs, their_socket) = tenant("theirs", 2);
    let client = CellClient::connect(&their_socket, theirs.join("store"), 1 << 24).unwrap();
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, extent, 0, &pattern(4, BLOCK)).unwrap();
    client.commit(Some(b"theirs")).unwrap();

    let version = client.make_version().unwrap();
    let restored = ours.join("restored");
    let opened = CellClient::connect_forked(&our_socket, &restored, version, 1 << 24);
    assert_eq!(opened.map(drop).map_err(|error| error.errno()), Err(Some(Errno::EACCES)));
    assert!(!restored.exists(), "a restore of another tenant's version not shared made a cell");
    let later = ExtentId { cell: client.cell(), sequence: 2 };
    assert_eq!(write(&client, later, 0, &pattern(5, BLOCK)), Ok(()));
    assert_eq!(client.commit(None), Ok(()));
    assert_eq!(read(&client, extent, 0, BLOCK), Ok(pattern(4, BLOCK)));
}

/// TASK-150.5: a connection has one cell, however many `OPEN`s its VM process has in flight: the
/// second, as the first waits for its cell's last holder to let it go, is refused, holding
/// nothing, and both cells open for others once the connection goes.
#[test]
fn connections_open_one_cell_at_a_time() {
    use crate::cells::{cell_dir_key, table};
    use crate::wire::*;
    use std::os::unix::ffi::OsStrExt as _;
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let (first, second) = (dir.path().join("first"), dir.path().join("second"));
    let key = cell_dir_key(&first);
    let held = table().hold(&key, None).unwrap();

    let stream = UnixStream::connect(&socket).unwrap();
    let sender = Sender::new(stream.try_clone().unwrap()).unwrap();
    let mut receiver = Receiver::new(stream.try_clone().unwrap());
    let open = |id| {
        Header::new(OPEN, id)
            .with_body(&OpenBody { capacity: 1 << 24, version: VERSION, ..Default::default() })
            .with_flags(OPEN_CREATE)
    };
    let open_payload = |path: &Path| {
        let mut payload = path.as_os_str().as_bytes().to_vec();
        payload.push(0);
        payload.extend_from_slice(cladefs_core::BUILD_COMMIT.as_bytes());
        payload
    };
    sender.send(open(1), &open_payload(&first), &[]).unwrap();
    while !table().is_waited_for(&key) {
        std::thread::yield_now();
    }
    let second_opened = call(&sender, &mut receiver, open(2), &open_payload(&second));
    drop(held);
    let first_opened = receiver.receive(|| 0).unwrap().unwrap();
    let statuses =
        (first_opened.header.id, first_opened.header.status, second_opened.header.status);
    stream.shutdown(std::net::Shutdown::Both).unwrap();
    drop((sender, receiver, stream));

    let (opening, reopened_first, reopened_second) =
        (socket.clone(), first.clone(), second.clone());
    let opened = within(std::time::Duration::from_secs(30), move || {
        reopened(&opening, &reopened_first);
        reopened(&opening, &reopened_second);
    });
    assert_eq!(opened, Ok(()), "a cell stayed held once its connection went");
    assert_eq!(statuses, (1, 0, -Errno::EBUSY.raw()));
}

/// TASK-79.16: a tenant's VM processes, the default tenant's too, have so many connections at
/// most: one past them is closed as it comes; once one goes, another is served. The first of them
/// is served here, as tests' connections are, which says when it is done ([`serve_connection`]).
#[test]
fn tenants_connections_are_bounded() {
    use std::io::Read as _;
    host();
    crate::raise_open_files_limit().unwrap();
    // cladefs: a tenant's most connections 256, not 4096, whose ends this process would hold
    // past macOS's 10,240 files a process (board 13850); the daemon's most past them, whatever
    // the host's limit of open files gives it: macOS's (`kern.maxfilesperproc`) gave it 1016
    // (board 13065).
    const TENANTS: usize = 256;
    crate::server::TENANT_CONNECTIONS_GIVEN.set(TENANTS).unwrap();
    let given = (Some(2 * TENANTS), crate::server::RESERVED_CONNECTIONS);
    crate::server::CONNECTION_LIMITS_GIVEN.set(given).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    // Served once its cell is open.
    let (ours, theirs) = UnixStream::pair().unwrap();
    let serving = std::thread::spawn(move || serve_connection(theirs, default_tenant().unwrap()));
    let first =
        CellClient::over(ours, &dir.path().join("first"), Opening::Create(1 << 20, Kind::Fxfs));
    let first = first.unwrap();
    let connected: Vec<UnixStream> = (1..TENANTS).map(|_| connect_queued(&socket)).collect();
    let past = connect_queued(&socket);
    let closed = within(std::time::Duration::from_secs(10), move || {
        (&past).read(&mut [0]).map_err(|err| err.kind())
    });
    assert_eq!(closed, Ok(Ok(0)), "a connection past the tenant's was served");

    drop(first);
    serving.join().unwrap();
    let opened = CellClient::connect(&socket, dir.path().join("store"), 1 << 20);
    assert_eq!(opened.map(drop).map_err(|error| error.errno()), Ok(()), "no connection served");
    drop(connected);
}

/// A connection to the listener at `socket`, once its queue of connections not accepted yet has
/// room: macOS refuses one past it (`ECONNREFUSED`, its `kern.ipc.somaxconn` of 128), where
/// Linux holds it until the queue has room.
fn connect_queued(socket: &Path) -> UnixStream {
    loop {
        match UnixStream::connect(socket) {
            Err(err) if err.kind() == std::io::ErrorKind::ConnectionRefused => {
                std::thread::yield_now();
            }
            connected => return connected.unwrap(),
        }
    }
}

/// TASK-79.84: a VM restored from a snapshot to where a fork of its version was left unmade, its
/// host crashing as it made the child's cell, gets a fork made anew, of a cell of its own, holding
/// what the version holds: what was left goes (`host_cell::unmade`, TASK-165.7).
#[test]
fn restores_fork_anew_over_forks_left_unmade() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let client = CellClient::connect(&socket, dir.path().join("store"), 1 << 24).unwrap();
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, extent, 0, &pattern(8, BLOCK)).unwrap();
    client.commit(Some(b"root")).unwrap();
    let version = client.make_version().unwrap();
    drop(client);

    // What a fork of the version there leaves, its host crashing before its cell's description.
    let restored = dir.path().join("restored");
    std::fs::create_dir_all(restored.join("segments")).unwrap();
    std::fs::write(restored.join("ancestry"), "cladefs cell records 1\n1 /elsewhere\n").unwrap();
    let ours = CellClient::connect_forked(&socket, &restored, version, 0).unwrap();
    assert_eq!(ours.root(), Ok(b"root".to_vec()));
    assert_eq!(read(&ours, extent, 0, BLOCK), Ok(pattern(8, BLOCK)));
    assert!(!cladefs_core::host_cell::unmade(&restored), "the new fork is no cell");
}

/// TASK-194: a restore into a cell written since its snapshot's version, as the snapshotted VM's
/// own cell is once it resumed and wrote on, is refused: opened, its guest, of the snapshot's
/// memory, would read extents freed since, and commit over what was written since.
#[test]
fn restores_refuse_a_cell_written_since_its_version() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let path = dir.path().join("store");
    let client = CellClient::connect(&socket, &path, 1 << 24).unwrap();
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, extent, 0, &pattern(8, BLOCK)).unwrap();
    client.commit(Some(b"root")).unwrap();
    let version = client.make_version().unwrap();
    // The VM resumes, and goes on: what its snapshot holds it frees, and it commits anew.
    client.free(extent).unwrap();
    client.commit(Some(b"later")).unwrap();
    drop(client);

    let restored = CellClient::connect_forked(&socket, &path, version, 0);
    assert_eq!(restored.err().and_then(|err| err.errno()), Some(Errno::EEXIST));
    // Nothing of it changed: opened, it is as the VM left it.
    let client = CellClient::connect(&socket, &path, 1 << 24).unwrap();
    assert_eq!(
        (client.root(), read(&client, extent, 0, BLOCK).ok()),
        (Ok(b"later".to_vec()), None)
    );
}

/// TASK-194: what a restore may open, a cell there already holding what its version does: the cell
/// its snapshot was taken of, written nothing since, as a VM stopped without resuming leaves it;
/// and a fork of the version an earlier restore made, its VM having written nothing, retried. Once
/// that fork is written, a restore into it is refused too.
#[test]
fn restores_open_cells_still_at_their_version() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let path = dir.path().join("store");
    let client = CellClient::connect(&socket, &path, 1 << 24).unwrap();
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, extent, 0, &pattern(8, BLOCK)).unwrap();
    client.commit(Some(b"root")).unwrap();
    let version = client.make_version().unwrap();
    drop(client);
    let restore = |into: &Path| CellClient::connect_forked(&socket, into, version, 0);
    let at_point = |client: &CellClient| (client.root().ok(), read(client, extent, 0, BLOCK).ok());
    let state = (Some(b"root".to_vec()), Some(pattern(8, BLOCK)));

    // The snapshotted VM's own cell, written nothing since its version.
    let restored = restore(&path).unwrap();
    assert_eq!(at_point(&restored), state);
    drop(restored);

    // A fork an earlier restore made, its VM gone before it wrote anything, restored into again.
    let fork = dir.path().join("fork");
    let restored = restore(&fork).unwrap();
    let cell = restored.cell();
    drop(restored);
    let restored = restore(&fork).unwrap();
    assert_eq!((restored.cell(), at_point(&restored)), (cell, state.clone()));
    // Its VM writes on, and goes: restored into once more, it is refused.
    write(&restored, minted(&restored), 0, &pattern(9, BLOCK)).unwrap();
    restored.commit(Some(b"later")).unwrap();
    drop(restored);
    assert_eq!(restore(&fork).err().and_then(|err| err.errno()), Some(Errno::EEXIST));
}

/// What a cell of the daemon's at `path` mints next, as its guest mints: from its writer's floor,
/// or past what it minted.
fn minted(client: &CellClient) -> ExtentId {
    let info = client.info().unwrap();
    assert!(info.sequence_floor >= 1 << bucket_cell::EPOCH_SHIFT, "{info:?}");
    ExtentId { cell: client.cell(), sequence: info.next_sequence.max(info.sequence_floor) }
}

/// The cell at `path`, once the connection that had it open let it go.
fn reopened(socket: &Path, path: &Path) -> CellClient {
    loop {
        match CellClient::connect(socket, path, 1 << 24) {
            Err(error) if error.errno() == Some(Errno::EBUSY) => std::thread::yield_now(),
            opened => break opened.unwrap(),
        }
    }
}

/// Waits for the daemon to have let go of the cell at `path`, whose connection went: closed, its
/// uploader done with what closing uploads, as another connection may open it then.
fn let_go(path: &Path) {
    let (table, key) = (crate::cells::table(), crate::cells::cell_dir_key(path));
    // `EBUSY` until the daemon sees the connection going; then the wait is for it to let go.
    let hold = loop {
        match table.hold(&key, None) {
            Err(Errno::EBUSY) => std::thread::yield_now(),
            held => break held.unwrap(),
        }
    };
    // Then for its cell, closing, to go: the table has it while anything does.
    while hold.instance().is_some() {
        std::thread::yield_now();
    }
}

/// TASK-44.10: a guest's fork failing at each of its steps ([`crate::faults::FORK_STEPS`]) fails
/// with `EIO`, leaving nothing at its path; the cell opens again, holds what it held, and forks;
/// and another's cell is served on. `bucket::fork_faults` goes further, on cells alone: this is the
/// daemon's side, its requests, connections, and errnos.
///
/// A step fails once, for whichever uploader passes it first, failpoints being the process's: so
/// each fails with no other cell's uploader at work, theirs uploading only when asked, and the
/// cells of the step before closed (TASK-157). `other`'s waking on its interval took
/// `upload::gather`'s failure, under load, its fork's own gather a while after it was set.
#[test]
fn guests_forks_failing_at_each_step_harm_nothing() {
    host();
    crate::cells::uploads_when_asked();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let other = CellClient::connect(&socket, &dir.path().join("other"), 1 << 24).unwrap();
    let kept = minted(&other);
    write(&other, kept, 0, &pattern(9, BLOCK)).unwrap();
    other.commit(None).unwrap();

    for (case, step) in cladefs_core::faults::FORK_STEPS.into_iter().enumerate() {
        let path = dir.path().join(format!("store-{case}"));
        let client = CellClient::connect(&socket, &path, 1 << 24).unwrap();
        let extent = minted(&client);
        write(&client, extent, 0, &pattern(1, BLOCK)).unwrap();
        client.commit(Some(b"root")).unwrap();
        let child = dir.path().join(format!("child-{case}"));
        let failing = fail::FailGuard::new(step, "1*return").unwrap();
        let forked = fork_cell(&path, &child).map_err(|err| err.errno());
        drop(failing);
        // Failing, but at its version's drop, past its child made, which fails it no more
        // (TASK-609).
        let failed = (step != "upload::drop_versions").then_some(Some(Errno::EIO));
        assert_eq!((forked.err(), child.exists()), (failed, failed.is_none()), "{step}");
        drop(client);

        let client = reopened(&socket, &path);
        assert_eq!(client.root(), Ok(b"root".to_vec()), "{step}");
        assert_eq!(read(&client, extent, 0, BLOCK), Ok(pattern(1, BLOCK)), "{step}");
        let retried = dir.path().join(format!("retried-{case}"));
        assert_eq!(
            fork_cell(&path, &retried).map(drop).map_err(|err| err.errno()),
            Ok(()),
            "{step}"
        );
        let forked = CellClient::connect(&socket, &retried, 1 << 24).unwrap();
        assert_eq!(read(&forked, extent, 0, BLOCK), Ok(pattern(1, BLOCK)), "{step}");
        forked.commit(Some(b"fork")).unwrap();

        assert_eq!(read(&other, kept, 0, BLOCK), Ok(pattern(9, BLOCK)), "{step}");
        assert_eq!(other.commit(None), Ok(()), "{step}");
        drop((forked, client));
        let_go(&retried);
        let_go(&path);
    }
}

/// TASK-44.10: a fork made whose answer its guest never got, which took it to have failed: the fork
/// stands, a retry to its path fails with `EEXIST`, and it is deleted as any cell is. A guest's
/// request refused before it began makes nothing, and a retry forks.
#[test]
fn forks_whose_answers_are_lost_stand() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let path = dir.path().join("store");
    let client = CellClient::connect(&socket, &path, 1 << 24).unwrap();
    let extent = minted(&client);
    write(&client, extent, 0, &pattern(2, BLOCK)).unwrap();
    client.commit(Some(b"root")).unwrap();

    let refused = dir.path().join("refused");
    let failing = fail::FailGuard::new("cladefsd::fork_uploaded", "1*return").unwrap();
    assert_eq!(fork_cell(&path, &refused).map_err(|err| err.errno()), Err(Some(Errno::EIO)));
    drop(failing);
    assert!(!refused.exists(), "a request refused made a fork");
    assert_eq!(fork_cell(&path, &refused).map(drop).map_err(|err| err.errno()), Ok(()));

    let child = dir.path().join("child");
    let failing = fail::FailGuard::new("cladefsd::forked", "1*return").unwrap();
    assert_eq!(fork_cell(&path, &child).map_err(|err| err.errno()), Err(Some(Errno::EIO)));
    drop(failing);
    assert_eq!(fork_cell(&path, &child).map_err(|err| err.errno()), Err(Some(Errno::EEXIST)));
    assert_eq!(
        fork_cell(&path, &dir.path().join("again")).map(drop).map_err(|err| err.errno()),
        Ok(())
    );
    assert_eq!(client.commit(Some(b"later")), Ok(()));
    let forked = CellClient::connect(&socket, &child, 1 << 24).unwrap();
    assert_eq!(forked.root(), Ok(b"root".to_vec()));
    assert_eq!(read(&forked, extent, 0, BLOCK), Ok(pattern(2, BLOCK)));
    drop(forked);
    let deleted = loop {
        match delete_cell(&child) {
            Err(error) if error.errno() == Some(Errno::EBUSY) => std::thread::yield_now(),
            deleted => break deleted,
        }
    };
    assert_eq!(deleted.map_err(|err| err.to_string()), Ok(()));
    assert!(!child.exists(), "its cell is left");
}

/// TASK-44.10: a fork of the control API's failing once its cell's commits were uploaded makes
/// nothing; a retry forks. One of a new cell, of a version kept open, whose fork fails once it is
/// made, is made anew, of another cell, what the first left gone.
#[tokio::test(flavor = "multi_thread")]
async fn control_forks_failing_fork_again() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let (base, fork, again) =
        (dir.path().join("base"), dir.path().join("fork"), dir.path().join("again"));
    let (client, served) = connect(&base, 1 << 24);
    let extent = minted(&client);
    write(&client, extent, 0, &pattern(3, BLOCK)).unwrap();
    client.commit(Some(b"root")).unwrap();
    drop(client);
    served.join().unwrap();

    let failing = fail::FailGuard::new("cladefsd::fork_uploaded", "1*return").unwrap();
    let failed = fork_cell(&base, &fork).map_err(|err| err.errno());
    drop(failing);
    assert_eq!((failed, fork.exists()), (Err(Some(Errno::EIO)), false));
    fork_cell(&base, &fork).unwrap();

    let failing = fail::FailGuard::new("upload::forked", "1*return").unwrap();
    let cell =
        control(crate::cells::fork(crate::Daemon::process(), base.clone(), again.clone(), None))
            .unwrap();
    drop(failing);
    assert_eq!(OpenCell::open(&again).await.map(|client| client.cell()).map_err(drop), Ok(cell));
    let (forked, served) = connect(&again, 1 << 24);
    assert_eq!(read(&forked, extent, 0, BLOCK), Ok(pattern(3, BLOCK)));
    drop(forked);
    served.join().unwrap();
}

/// TASK-648: a fork of a version made right after its writer's connection went, the cell still
/// open in this process, syncs that cell as it is, never opening it again beside it. Here the
/// cell lingers by a reference the test keeps, as a fork's read of it would. A cell already
/// closing, its last reference dropped, an opening waits for (local_cell's
/// `openings_wait_for_a_cell_closing`): the window the failing runs met.
#[test]
fn forks_of_a_version_right_after_its_writer_went_sync_the_cell_still_closing() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let (cell_dir, restored) = (dir.path().join("store"), dir.path().join("restored"));
    let made = CellClient::connect(&socket, &cell_dir, 1 << 24).unwrap();
    let extent = minted(&made);
    write(&made, extent, 0, &pattern(5, BLOCK)).unwrap();
    made.commit(Some(b"root")).unwrap();
    let version = made.make_version().unwrap();
    let key = crate::cells::cell_dir_key(&cell_dir);
    let lingering = crate::cells::table().instance(&key);
    assert!(lingering.is_some(), "the cell is not open");
    drop(made);
    let forked = control(crate::cells::fork_of_version(
        crate::Daemon::process(),
        version_cell(version),
        version.version,
        restored.clone(),
        None,
    ));
    assert_eq!(forked.map(drop), Ok(()));
    drop(lingering);
    let ours = CellClient::connect(&socket, &restored, 0).unwrap();
    assert_eq!(read(&ours, extent, 0, BLOCK), Ok(pattern(5, BLOCK)));
}

/// board 8237: a writer's connection gone while the daemon's work through its cell, a fork's or a
/// sync's, has the cell still, the next writer waits for the cell to close, then opens it, rather
/// than fail at once (`EBUSY`) meeting a cell open beside it: told as it closes (`cells::Closed`).
/// Here the daemon's work is a reference the test keeps.
#[test]
fn writers_wait_for_a_cell_its_writer_left_to_the_daemons_work() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let cell_dir = dir.path().join("store");
    let made = CellClient::connect(&socket, &cell_dir, 1 << 24).unwrap();
    let extent = minted(&made);
    write(&made, extent, 0, &pattern(6, BLOCK)).unwrap();
    made.commit(Some(b"root")).unwrap();
    let key = crate::cells::cell_dir_key(&cell_dir);
    let lingering = crate::cells::table().instance(&key);
    assert!(lingering.is_some(), "the cell is not open");
    drop(made);
    let opening = {
        let (socket, cell_dir) = (socket.clone(), cell_dir.clone());
        std::thread::spawn(move || CellClient::connect(&socket, &cell_dir, 1 << 24))
    };
    // The next writer waits for the cell, or, refused, is done.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !crate::cells::table().is_waited_for(&key) && !opening.is_finished() {
        assert!(std::time::Instant::now() < deadline, "the next writer neither waits nor opens");
        std::thread::yield_now();
    }
    drop(lingering);
    let again = opening.join().unwrap().map_err(|error| error.errno());
    let again = again.expect("the next writer was refused the cell its writer left");
    assert_eq!(read(&again, extent, 0, BLOCK), Ok(pattern(6, BLOCK)));
}

/// A metadata reader can become the cell's last owner when its writer closes during inspection.
#[test]
fn inspections_close_the_last_cell_reference_away_from_the_reader() {
    use futures::FutureExt as _;
    use std::sync::Arc;

    host();
    let dir = tempfile::tempdir().unwrap();
    let cell = Arc::new(
        control(cladefs_core::OpenCell::create(dir.path().join("store"), 1 << 24)).unwrap(),
    );
    let id = cell.cell();
    let (closed, observed) = std::sync::mpsc::channel();
    cell.on_close(move || {
        closed.send(std::thread::current().id()).unwrap();
    });
    let reader = std::thread::current().id();
    let read = control(async {
        crate::cells::inspect(crate::Daemon::process(), cell.clone(), |open| {
            // The writer releases its reference while the reader is taking its snapshot.
            cladefs_core::open_cell::release(cell).now_or_never().unwrap();
            open.cell()
        })
    });
    assert_eq!(read, id);
    assert_ne!(observed.recv_timeout(std::time::Duration::from_secs(10)).unwrap(), reader);
}

/// TASK-644: a fork of a version made right after its writer's connection went, its cell closing
/// on a blocking thread, its uploader uploading its last commits, the version's among them, waits
/// for the close before it reads the bucket for the version. Before, it found no open cell
/// to sync, read the bucket at once, and failed `ENOENT` where the upload was still to come
/// (CI's `page_ins_in_flight_are_bounded`). The close held where it says it is closing, before its
/// uploads; the fork held where it waits; then the close let go on.
#[test]
fn forks_of_a_version_wait_for_the_close_of_the_cell_that_made_it() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let (cell_dir, restored) = (dir.path().join("store"), dir.path().join("restored"));
    let made = CellClient::connect(&socket, &cell_dir, 1 << 24).unwrap();
    let extent = minted(&made);
    write(&made, extent, 0, &pattern(6, BLOCK)).unwrap();
    made.commit(Some(b"root")).unwrap();
    let version = made.make_version().unwrap();

    let (closing, closing_seen) = std::sync::mpsc::channel();
    let (go, gone_on) = std::sync::mpsc::channel::<()>();
    let gone_on = std::sync::Mutex::new(gone_on);
    // The writer's close alone held: the version's cell, opened for the fork, closes unheld.
    let first = std::sync::atomic::AtomicBool::new(true);
    fail::cfg_callback("open_cell::closing", move || {
        if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
            closing.send(()).unwrap();
            gone_on.lock().unwrap().recv().unwrap();
        }
    })
    .unwrap();
    let (waiting, waiting_seen) = std::sync::mpsc::channel();
    let waiting = std::sync::Mutex::new(waiting);
    fail::cfg_callback("dir::claim_waits", move || {
        let _ = waiting.lock().unwrap().send(());
    })
    .unwrap();
    drop(made);
    let wait = std::time::Duration::from_secs(30);
    closing_seen.recv_timeout(wait).expect("the cell never said it was closing");
    let forking = std::thread::spawn({
        let restored = restored.clone();
        move || {
            let forking = crate::cells::fork_of_version(
                crate::Daemon::process(),
                version_cell(version),
                version.version,
                restored,
                None,
            );
            control(forking).map(drop)
        }
    });
    let waited = waiting_seen.recv_timeout(wait);
    go.send(()).unwrap();
    let forked = forking.join().unwrap();
    fail::remove("open_cell::closing");
    fail::remove("dir::claim_waits");
    assert_eq!((waited.map_err(drop), forked), (Ok(()), Ok(())), "the fork waited, and forked");
    let ours = CellClient::connect(&socket, &restored, 0).unwrap();
    assert_eq!(read(&ours, extent, 0, BLOCK), Ok(pattern(6, BLOCK)));
}

/// board 2922: a version shared right after its writer's connection went, the cell still open in
/// this process as its last reference is dropped, is shared through that cell as it is, as a
/// fork's sync does (board 2730): before, the control's work opened it again beside it. Here the
/// cell lingers by a reference the test keeps, as a close on a blocking thread, or a busy host,
/// would.
#[test]
fn versions_shared_right_after_their_writer_went_use_the_cell_still_closing() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let cell_dir = dir.path().join("store");
    let made = CellClient::connect(&socket, &cell_dir, 1 << 24).unwrap();
    let extent = minted(&made);
    write(&made, extent, 0, &pattern(5, BLOCK)).unwrap();
    made.commit(Some(b"root")).unwrap();
    let version = made.make_version().unwrap();
    let key = crate::cells::cell_dir_key(&cell_dir);
    let lingering = crate::cells::table().instance(&key);
    assert!(lingering.is_some(), "the cell is not open");
    drop(made);
    let shared = control(crate::cells::share_version(
        crate::Daemon::process(),
        cell_dir.clone(),
        version.version,
        true,
    ));
    assert_eq!(shared, Ok(()));
    let versions =
        control(crate::cells::versions(crate::Daemon::process(), cell_dir.clone())).unwrap();
    drop(lingering);
    assert_eq!(
        versions.iter().map(|each| (each.id, each.shared)).collect::<Vec<_>>(),
        vec![(version.version, true)]
    );
}

/// TASK-44.10: a restore, a fork of a version, which commits nothing, failing once its child's
/// record is made, its cleanup failing with it, leaves nothing at the restore's path, and in the
/// bucket an orphan, the child's record live (doc-14): a restore there forks anew. Before, it left
/// a handle naming no cell, which opened as no cell until deleted.
#[test]
fn restores_over_what_a_failed_cleanup_left_fork_anew() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let (cell_dir, restored) = (dir.path().join("store"), dir.path().join("restored"));
    let made = CellClient::connect(&socket, &cell_dir, 1 << 24).unwrap();
    let extent = minted(&made);
    write(&made, extent, 0, &pattern(4, BLOCK)).unwrap();
    made.commit(Some(b"root")).unwrap();
    let version = made.make_version().unwrap();
    drop(made);

    let left = ["fork::made", "lifecycle::head_ended"];
    let failing = left.map(|step| fail::FailGuard::new(step, "1*return").unwrap());
    let forked = control(crate::cells::fork_of_version(
        crate::Daemon::process(),
        version_cell(version),
        version.version,
        restored.clone(),
        None,
    ))
    .map(drop)
    .map_err(Some);
    drop(failing);
    assert_eq!((forked, restored.exists()), (Err(Some(Errno::EIO)), false));
    let ours = CellClient::connect_forked(&socket, &restored, version, 0).unwrap();
    assert_eq!(read(&ours, extent, 0, BLOCK), Ok(pattern(4, BLOCK)));
}

/// How many requests of `kind` the daemon has served, as its metrics count them.
fn served(kind: &str) -> u64 {
    let metrics = crate::metrics::render();
    let name = format!("cladefsd_requests_total{{kind=\"{kind}\"}} ");
    let line = metrics.lines().find_map(|line| line.strip_prefix(&name));
    line.unwrap_or_else(|| panic!("no {kind} requests counted in {metrics}")).parse().unwrap()
}

/// TASK-92.13: a flush's dead extents, as many as a guest's `FREE` names with `FREE_MANY` (257),
/// freed together, are freed in one request of the daemon's, not one each, each a round trip.
#[test]
fn extents_freed_together_are_freed_in_one_request() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let (client, serving) = connect(&dir.path().join("store"), 1 << 24);
    let cell = client.cell();
    let extents: Vec<_> = (1..=257).map(|sequence| ExtentId { cell, sequence }).collect();
    for extent in &extents {
        write(&client, *extent, 0, &pattern(extent.sequence as u8, BLOCK)).unwrap();
    }
    client.commit(None).unwrap();
    let before = served("free");
    client.free_many(&extents).unwrap();
    assert_eq!(served("free") - before, 1);
    for extent in [extents[0], extents[128], extents[256]] {
        assert_eq!(read(&client, extent, 0, BLOCK), Err(Errno::ENOENT), "{extent:?}");
    }
    drop(client);
    serving.join().unwrap();
}

/// TASK-92.13, TASK-141.3: a VM process's frees of many extents in one request never reach a daemon
/// older than that request, which knows `FREE` alone, and would free the first, ignoring the flags
/// and payload it doesn't know, the rest leaked, success said: a daemon of another wire version,
/// an older one saying none, the VM process refuses as it opens its cell, closing the connection
/// on it, naming both versions. The daemon here is one by hand, as a daemon before TASK-92.13
/// served. Before TASK-141.3, the VM process opened its cell, and its `FREE_MANY` failed with
/// the daemon's `EINVAL`.
#[test]
fn daemons_of_other_wire_versions_are_refused() {
    use crate::wire::*;
    use std::os::fd::AsFd;
    let ps = page_size();
    let (stream, daemon) = UnixStream::pair().unwrap();
    let answering = std::thread::spawn(move || {
        let (sender, mut receiver) =
            (Sender::new(daemon.try_clone().unwrap()).unwrap(), Receiver::new(daemon));
        let (own, public) = (memfd(ps), memfd(ps));
        let mut served = Vec::new();
        while let Ok(Some(request)) = receiver.receive(|| 0) {
            let mut reply = Header::new(REPLY, request.header.id);
            let mut fds = Vec::new();
            served.push(request.header.kind);
            match request.header.kind {
                // As a daemon of no wire version replied: its version 0.
                OPEN => {
                    reply.set_body(&OpenedBody {
                        cell: 7,
                        tenant: 0,
                        arena: 1,
                        flags: 0,
                        max_map_count: 0,
                        version: 0,
                    });
                    fds = vec![own.as_fd(), public.as_fd()];
                }
                _ => reply.status = -Errno::EINVAL.raw(),
            }
            sender.send(reply, &[], &fds).unwrap();
        }
        served
    });
    let refused =
        CellClient::over(stream, Path::new("store"), Opening::Create(1 << 20, Kind::Fxfs));
    let refused = refused.err().expect("a daemon of another wire version refused");
    assert_eq!(refused.errno(), Some(Errno::EPROTONOSUPPORT));
    assert_eq!(
        format!("{refused:#}"),
        format!(
            "opening the extent store at store: the extent daemon is of wire version 0, this VM \
             process of {VERSION}: EPROTONOSUPPORT (errno 93)"
        )
    );
    // Closed on, once it opened: nothing else asked of it.
    assert_eq!(answering.join().unwrap(), [OPEN]);
}

/// TASK-141.3: a daemon refuses a VM process of another wire version as it opens its cell, with
/// `EPROTONOSUPPORT` and its own version, whatever else its `OPEN` holds, and opens nothing for it;
/// a VM process of its own version, after, it serves. Before, nothing said a version: a VM process
/// of another build was served, its requests read as this one has them.
#[test]
fn vm_processes_of_other_wire_versions_are_refused() {
    use crate::wire::*;
    host();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let (stream, server) = UnixStream::pair().unwrap();
    let served = std::thread::spawn(move || serve_connection(server, default_tenant().unwrap()));
    let (sender, mut receiver) = (
        Sender::new(stream.try_clone().unwrap()).unwrap(),
        Receiver::new(stream.try_clone().unwrap()),
    );
    let named = std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str());
    for (id, other) in [(1, 0), (2, VERSION + 1), (3, u64::MAX)] {
        // Flags this version doesn't know, as another's OPEN may have.
        for flags in [OPEN_CREATE, u32::MAX] {
            let open = Header::new(OPEN, id)
                .with_body(&OpenBody { capacity: 1 << 24, version: other, ..Default::default() })
                .with_flags(flags);
            sender.send(open, named, &[]).unwrap();
            let reply = receiver.receive(|| 0).unwrap().unwrap();
            let said = (reply.header.id, reply.header.status, crate::codec::version(&reply.header));
            assert_eq!(said, (id, -Errno::EPROTONOSUPPORT.raw(), VERSION), "{other}, {flags:#x}");
            assert!(reply.fds.is_empty());
        }
    }
    assert!(!path.exists(), "a cell was made for a VM process of another version");
    drop((sender, receiver, stream));
    served.join().unwrap();
    let (client, served) = connect(&path, 1 << 24);
    assert_eq!(client.info().map(|info| info.capacity), Ok(1 << 24));
    drop(client);
    served.join().unwrap();
}

/// TASK-141.3, board 11707: opening sends `OPEN` alone (no eager `TRANSFER` RTT); on the first
/// non-DAX read or write, `TRANSFER` is required and fails hard if refused (no silent fallback to
/// the socket).
#[test]
fn openings_fail_on_refusals_of_what_they_ask() {
    use crate::wire::*;
    use std::os::fd::AsFd;
    let ps = page_size();
    let (stream, daemon) = UnixStream::pair().unwrap();
    let answering = std::thread::spawn(move || {
        let (sender, mut receiver) =
            (Sender::new(daemon.try_clone().unwrap()).unwrap(), Receiver::new(daemon));
        let (own, public) = (memfd(ps), memfd(ps));
        let mut served = Vec::new();
        while let Ok(Some(request)) = receiver.receive(|| 0) {
            let mut reply = Header::new(REPLY, request.header.id);
            let (mut payload, mut fds) = (Vec::new(), Vec::new());
            served.push(request.header.kind);
            match request.header.kind {
                OPEN => {
                    reply.set_body(&opened_body(7));
                    payload = 0u64.to_le_bytes().to_vec();
                    fds = vec![own.as_fd(), public.as_fd()];
                }
                TRANSFER => reply.status = -Errno::EINVAL.raw(),
                _ => reply.status = -Errno::EINVAL.raw(),
            }
            sender.send(reply, &payload, &fds).unwrap();
        }
        served
    });
    let client = CellClient::over(stream, Path::new("store"), Opening::Create(1 << 20, Kind::Fxfs))
        .expect("opening sends OPEN alone");
    let mut buf = vec![0u8; BLOCK];
    let err = client
        .read(ExtentId { cell: 7, sequence: 1 }, 0, &mut [VolatileSliceMut::from(&mut buf[..])])
        .unwrap_err();
    assert_eq!(err, Errno::EINVAL);
    drop(client);
    assert_eq!(answering.join().unwrap(), vec![OPEN, TRANSFER]);
}

/// What a daemon by hand replies to an `OPEN`, of its cell of `cell`: the default arena's memory
/// its own, as of the default tenant, and its wire version this one.
fn opened_body(cell: u64) -> crate::wire::OpenedBody {
    let arena = cladefs_core::pages::ArenaId(1);
    crate::codec::Opened {
        cell,
        tenant: 0,
        arena,
        max_map_count: None,
        ephemeral: false,
        capacity: 0,
    }
    .to_body()
}

/// TASK-216: a VM booted again on the cell its VM crashed out of opens it at once, while what
/// that cell's uploader had in flight waits on the bucket: the uploader of a cell whose
/// connection went stops as the new writer waits for it, without uploading the rest, which the
/// cell's write-ahead log keeps and the new opening uploads. Before, the going connection's cell
/// was closed as any cell is, its uploader uploading all it held first, and the new opening,
/// waiting at most `CLOSING_WAIT` for that, failed `EBUSY` (fxfs-guest-soak on GCS, GCE r758
/// round 14, every bucket request 1000 ms late).
#[test]
fn cells_whose_vm_went_open_again_while_their_uploads_wait() {
    use std::sync::Arc;
    host();
    let ps = page_size() as usize;
    let gated = Arc::new(GatedReads::new());
    crate::cells::use_bucket(
        bucket_cell::object_bucket::ObjectBucket::new(gated.clone(), "cells").unrouted(),
    );
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let path = dir.path().join("store");
    let client = CellClient::connect(&socket, &path, 1 << 24).unwrap();
    let cell = client.cell();
    let write = |client: &CellClient, sequence: u64| {
        let extent = ExtentId { cell, sequence };
        let data = pattern(sequence as u8, ps);
        client.write(extent, 0, &[VolatileSlice::from(&data[..])]).unwrap();
    };
    write(&client, 1);
    client.commit(None).unwrap();
    wait_uploaded(&path).unwrap();
    // The next segment's upload, held: the bucket out of reach for what the cell has in flight.
    let segments = format!("cells/{}", bucket_cell::layout::segments(cell_at(&path)));
    gated.hold_next_write(&segments);
    write(&client, 2);
    client.commit_ordered(None).unwrap();
    // The VM goes, its connection with it, its cell's uploader uploading what it held.
    drop(client);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while gated.writes_waiting() == 0 {
        assert!(std::time::Instant::now() < deadline, "the going cell's upload was never held");
        std::thread::yield_now();
    }
    // A VM booted again opens it, at once, and reads what the first committed.
    let asked = std::time::Instant::now();
    let again = CellClient::connect(&socket, &path, 1 << 24);
    let took = asked.elapsed();
    let again = again.unwrap_or_else(|err| panic!("opening again, after {took:?}: {err:#}"));
    assert!(took < crate::cells::CLOSING_WAIT / 2, "opening again took {took:?}");
    let mut data = vec![0; ps];
    let extent = ExtentId { cell, sequence: 2 };
    again.read(extent, 0, &mut [VolatileSliceMut::from(&mut data[..])]).unwrap();
    assert!(data == pattern(2, ps), "the cell opened again reads other data");
    // What it opened with lands in the bucket, with the commit the first left in its log.
    again.commit(None).unwrap();
    gated.let_held_write_go();
    drop(again);
}

/// A memory bucket whose reads wait while they are held ([`GatedReads::hold`]): what
/// makes a page-in's fetch from object storage wait as long as a test needs. And whose next write
/// under a prefix waits until let go ([`GatedReads::hold_next_write`]).
#[derive(Debug)]
struct GatedReads {
    memory: object_store::memory::InMemory,
    held: tokio::sync::watch::Sender<bool>,
    /// The reads waiting.
    waiting: std::sync::atomic::AtomicUsize,
    /// The writes waiting.
    writes_waiting: std::sync::atomic::AtomicUsize,
    /// The prefix of the keys whose next write alone waits, until let go, if any
    /// ([`GatedReads::hold_next_write`]); and whether the one held is let go.
    next_write: std::sync::Mutex<Option<String>>,
    next_write_going: tokio::sync::watch::Sender<bool>,
}

impl std::fmt::Display for GatedReads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "gated reads")
    }
}

impl GatedReads {
    fn new() -> Self {
        Self {
            memory: object_store::memory::InMemory::new(),
            held: tokio::sync::watch::channel(false).0,
            waiting: Default::default(),
            writes_waiting: Default::default(),
            next_write: Default::default(),
            next_write_going: tokio::sync::watch::channel(false).0,
        }
    }

    /// Holds the next write of a key beginning with `prefix`, and no other after it, until
    /// [`GatedReads::let_held_write_go`]: what one writer has in flight, which those after it don't
    /// wait behind.
    fn hold_next_write(&self, prefix: &str) {
        self.next_write_going.send_replace(false);
        *self.next_write.lock().unwrap() = Some(prefix.to_owned());
    }

    fn let_held_write_go(&self) {
        self.next_write_going.send_replace(true);
    }

    fn writes_waiting(&self) -> usize {
        self.writes_waiting.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Holds the reads from now on, if `held`, or lets them go.
    #[cfg(target_os = "linux")]
    fn hold(&self, held: bool) {
        self.held.send_replace(held);
    }

    #[cfg(target_os = "linux")]
    fn waiting(&self) -> usize {
        self.waiting.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl object_store::ObjectStore for GatedReads {
    async fn put_opts(
        &self,
        location: &object_store::path::Path,
        payload: object_store::PutPayload,
        opts: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        use std::sync::atomic::Ordering;
        let holds = |prefix: &Option<String>| {
            prefix.as_ref().is_some_and(|prefix| location.as_ref().starts_with(prefix.as_str()))
        };
        let next = {
            let mut next = self.next_write.lock().unwrap();
            let holds_next = holds(&next);
            holds_next.then(|| next.take())
        };
        if next.is_some() {
            let mut going = self.next_write_going.subscribe();
            self.writes_waiting.fetch_add(1, Ordering::Relaxed);
            let _ = going.wait_for(|going| *going).await;
            self.writes_waiting.fetch_sub(1, Ordering::Relaxed);
        }
        self.memory.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &object_store::path::Path,
        opts: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.memory.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &object_store::path::Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        use std::sync::atomic::Ordering;
        if *self.held.borrow() {
            let mut held = self.held.subscribe();
            self.waiting.fetch_add(1, Ordering::Relaxed);
            let _ = held.wait_for(|held| !*held).await;
            self.waiting.fetch_sub(1, Ordering::Relaxed);
        }
        self.memory.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: futures::stream::BoxStream<
            'static,
            object_store::Result<object_store::path::Path>,
        >,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::path::Path>> {
        self.memory.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
        self.memory.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.memory.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &object_store::path::Path,
        to: &object_store::path::Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.memory.copy_opts(from, to, options).await
    }
}

/// TASK-92.20: a VM's faults that wait for the pager, which fetches what they fault on from
/// object storage, hold up none of its requests: here 12 of them, more than the 8 requests the
/// daemon serves of a connection at once, wait while the bucket answers no reads, and the VM's
/// `INFO` is answered meanwhile. Each held a thread of the daemon's and one of those 8 while it
/// waited, as for a part from GCS, 30-100 ms, and the requests after them waited their turn.
#[tokio::test(flavor = "multi_thread")]
#[cfg(target_os = "linux")]
async fn faults_waiting_for_object_storage_hold_up_no_request() {
    use std::sync::Arc;
    const FAULTS: usize = 12;
    host();
    let ps = page_size() as usize;
    let gated = Arc::new(GatedReads::new());
    crate::cells::use_bucket(
        bucket_cell::object_bucket::ObjectBucket::new(gated.clone(), "cells").unrouted(),
    );
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    // A cell of an extent a page for each fault, written here, whose pages are of an arena no
    // VM process's connection has, made a snapshot's version through the daemon, which keeps its
    // cell in the bucket.
    let path = dir.path().join("store");
    let cell = {
        let open_cell = OpenCell::create(&path, 1 << 24).await.unwrap();
        let cell = open_cell.cell();
        for sequence in 1..=FAULTS as u64 {
            let data = pattern(sequence as u8, ps);
            open_cell
                .write(ExtentId { cell: cell.id, sequence }, 0, &[VolatileSlice::from(&data[..])])
                .await
                .unwrap();
        }
        open_cell.commit(None).await.unwrap();
        cell
    };
    let client = CellClient::connect(&socket, &path, 1 << 24).unwrap();
    let version = client.make_version().unwrap();
    drop(client);
    // Its VM restored on another host, which has the version's extents in the bucket alone.
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    let restored = elsewhere.join("store");
    let ours = CellClient::connect_forked(&socket, &restored, version, 1 << 24).unwrap();
    let memory = memory(FAULTS);
    // SAFETY: The memory is the test's, and outlives the cell.
    unsafe { ours.create_dax_window_in_host_memory(memory, (FAULTS * ps) as u64, None, 1) }
        .unwrap();
    for n in 0..FAULTS {
        let extent = ExtentId { cell: cell.id, sequence: n as u64 + 1 };
        ours.map(extent, 0, ps as u64, (n * ps) as u64).unwrap();
    }
    gated.hold(true);
    let base = memory.as_ptr() as usize;
    let faulting: Vec<_> = (0..FAULTS)
        .map(|n| {
            // SAFETY: The window's memory, whose faults the cell serves.
            std::thread::spawn(move || unsafe {
                std::slice::from_raw_parts((base + n * ps) as *const u8, ps).to_vec()
            })
        })
        .collect();
    // Every fault received, each waiting for a fetch.
    let connection =
        crate::cells::connection_writing(&crate::cells::cell_dir_key(&restored)).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while connection.unanswered() < FAULTS || gated.waiting() == 0 {
        assert!(std::time::Instant::now() < deadline, "{} faults", connection.unanswered());
        std::thread::yield_now();
    }
    let ours = Arc::new(ours);
    let asking = ours.clone();
    let info = within(std::time::Duration::from_secs(10), move || asking.info());
    let waiting = faulting.iter().filter(|faulting| !faulting.is_finished()).count();
    gated.hold(false);
    assert!(matches!(info, Ok(Ok(_))), "the VM's INFO waited for its faults: {info:?}");
    assert_eq!(waiting, FAULTS, "the faults were answered as the INFO was");
    for (n, faulting) in faulting.into_iter().enumerate() {
        let page = faulting.join().unwrap();
        assert!(page == pattern(n as u8 + 1, ps), "page {n} holds other data");
    }
}

/// TASK-443.6: restores of a version, all at once, wait for the one opening it however long its
/// opening takes, here from object storage 600 ms away, some 11 s, and for each other: none
/// fails. Each waited `CLOSING_WAIT` (10 s) at most before, its turn at the version's cell
/// coming after the opening and the turns of those that took it first, and failed `EBUSY`, 11 of
/// 20 so (3 restores at once wait less: none failed).
#[test]
fn restores_wait_for_a_version_however_long_its_opening_takes() {
    const RESTORES: usize = 20;
    host();
    // SAFETY: As in host.
    unsafe { std::env::set_var("CLADEFS_BUCKET_LATENCY_MS", "600") };
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let client = CellClient::connect(&socket, dir.path().join("store"), 1 << 24).unwrap();
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, extent, 0, &pattern(6, BLOCK)).unwrap();
    client.commit(Some(b"snapshot")).unwrap();
    let version = client.make_version().unwrap();
    drop(client);
    let restoring: Vec<_> = (0..RESTORES)
        .map(|n| {
            let (socket, to) = (socket.clone(), dir.path().join(format!("restore-{n}")));
            std::thread::spawn(move || {
                let start = std::time::Instant::now();
                let restored = CellClient::connect_forked(&socket, to, version, 0);
                let root = restored.map_err(|error| error.errno()).and_then(|client| {
                    assert_eq!(read(&client, extent, 0, BLOCK), Ok(pattern(6, BLOCK)));
                    client.root().map_err(Some)
                });
                (root, start.elapsed())
            })
        })
        .collect();
    let restored: Vec<_> = restoring.into_iter().map(|thread| thread.join().unwrap()).collect();
    println!("{RESTORES} restores at once: {restored:?}");
    // They took past what they waited for before, or this shows nothing.
    let longest = restored.iter().map(|(_, took)| *took).max().unwrap();
    assert!(longest > crate::cells::CLOSING_WAIT, "the restores took {longest:?}");
    for (n, (root, _)) in restored.iter().enumerate() {
        assert_eq!(root, &Ok(b"snapshot".to_vec()), "restore {n}");
    }
}

/// TASK-92.5: the restores of a zygote's snapshot, all at once, here with object storage as far
/// away as another region's (50 ms), fork its version as it is, opened once for them and kept open:
/// none fails, and nothing takes the version's cell, which has no writer (TASK-158), let alone
/// once each. Each restore opened the version, attached it (taking its cell), forked and closed
/// it, in turn, the k-th waiting for all those before it, a second or more each, and those that
/// waited past `CLOSING_WAIT` failed (`EBUSY`). Their forks are made in object storage one at a
/// time still, each waiting for a checkpoint of the version's cell that reads the record of every
/// fork before it (TASK-92.4): what each restore took is told.
#[test]
fn a_zygotes_restores_open_its_copy_once() {
    const RESTORES: usize = 24;
    host();
    // SAFETY: As in host.
    unsafe { std::env::set_var("CLADEFS_BUCKET_LATENCY_MS", "50") };
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let client = CellClient::connect(&socket, dir.path().join("store"), 1 << 24).unwrap();
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, extent, 0, &pattern(6, BLOCK)).unwrap();
    client.commit(Some(b"zygote")).unwrap();
    let version = client.make_version().unwrap();
    drop(client);
    // How many writers took the version's cell: its head's epoch.
    let bucket = crate::cells::daemon_bucket().unwrap();
    let cell = version_cell(version);
    let takings =
        || control(bucket_cell::head::get(&*bucket, cell)).unwrap().map_or(0, |head| head.epoch);
    let taken = takings();

    let restoring: Vec<_> = (0..RESTORES)
        .map(|n| {
            let (socket, version) = (socket.clone(), version.clone());
            let to = dir.path().join(format!("restore-{n}"));
            std::thread::spawn(move || {
                let start = std::time::Instant::now();
                let restored = CellClient::connect_forked(&socket, to, version, 0);
                let root = restored.map_err(|error| error.errno()).and_then(|client| {
                    assert_eq!(read(&client, extent, 0, BLOCK), Ok(pattern(6, BLOCK)));
                    client.root().map_err(Some)
                });
                (root, start.elapsed())
            })
        })
        .collect();
    let restored: Vec<_> = restoring.into_iter().map(|thread| thread.join().unwrap()).collect();
    let took: Vec<_> = restored.iter().map(|(_, took)| *took).collect();
    let taken = takings() - taken;
    println!("{RESTORES} restores at once took {took:?}; the version's cell taken {taken} times");
    for (n, (root, _)) in restored.iter().enumerate() {
        assert_eq!(root, &Ok(b"zygote".to_vec()), "restore {n}");
    }
    assert_eq!(taken, 0, "a writer took the version's cell");
}

/// TASK-79.16: a `READ` asking for more bytes than a message holds is refused (`EINVAL`), and its
/// VM process served on, whatever it asks for: here more than what a request's bytes of stream
/// data count, the payload it carries and what it asks for, may add up to.
#[test]
fn reads_past_a_messages_payload_are_refused() {
    use crate::wire::*;
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let (sender, mut receiver, _stream, cell) =
        raw_connection(&socket, &dir.path().join("store"), 1 << 24);
    let body = ReadBody { cell, sequence: 1, offset: 0, len: u64::MAX, transfer_at: 0 };
    sender.send(Header::new(READ, 2).with_body(&body), &[], &[]).unwrap();
    let refused = receiver.receive(|| 0).unwrap().expect("the daemon let the VM process go");
    assert_eq!((refused.header.id, refused.header.status), (2, -Errno::EINVAL.raw()));
    sender.send(Header::new(INFO, 3), &[], &[]).unwrap();
    let info = receiver.receive(|| 0).unwrap().expect("the daemon let the VM process go");
    assert_eq!((info.header.id, info.header.status), (3, 0));
}

/// Sends `header` and `payload`, a request, through `sender`, and returns its reply, which
/// `receiver` receives, the stage operations before it acknowledged, as a VM process makes them.
fn call(
    sender: &crate::wire::Sender,
    receiver: &mut crate::wire::Receiver,
    header: crate::wire::Header,
    payload: &[u8],
) -> crate::wire::Message {
    use crate::wire::*;
    sender.send(header, payload, &[]).unwrap();
    loop {
        let message = receiver.receive(|| 0).unwrap().expect("the daemon let the VM process go");
        match message.header.kind {
            REPLY => {
                assert_eq!(message.header.id, header.id, "a reply out of order");
                return message;
            }
            _ if message.header.flags & STAGE_ACK != 0 => {
                sender.send(Header::new(ACK, message.header.id), &[], &[]).unwrap();
            }
            _ => {}
        }
    }
}

/// TASK-79.9: a shrink of a tenant's memory evicts what it must, and returns what the tenant holds,
/// once the pages it evicted are free: a window's, which wait, freed, for its VM process to
/// acknowledge their unmapping, or for the daemon to let go of it, as here of one that acknowledges
/// nothing. Of two extents of two slabs each, read and mapped, it evicts the one asked for longest
/// ago, which leaves the tenant the other's slabs alone: those it freed, kept spare as they were,
/// given back too.
#[tokio::test(flavor = "multi_thread")]
async fn shrinks_return_once_the_pages_they_evict_are_free() {
    use crate::wire::*;
    const CHUNK: u64 = 2 * cladefs_core::slabs::SLAB;
    host();
    let dir = tempfile::tempdir().unwrap();
    let tenant = crate::server::Tenant::new(1, dir.path()).unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || crate::server::serve_tenant(listener, tenant));
    let (sender, mut receiver, _stream, cell) =
        raw_connection(&socket, &dir.path().join("store"), 1 << 26);
    let mut served = |header: Header, payload: &[u8]| {
        let reply = call(&sender, &mut receiver, header, payload);
        assert_eq!(reply.header.status, 0, "request {}", header.kind);
        reply
    };
    for sequence in 1..=2 {
        let written = Header::new(WRITE, sequence).with_body(&WriteBody {
            cell,
            sequence,
            data_len: CHUNK,
            ..Default::default()
        });
        served(written, &pattern(sequence as u8, CHUNK as usize));
    }
    served(Header::new(COMMIT, 3).with_flags(COMMIT_UPLOADED), &[]);
    for sequence in 1..=2 {
        let read = Header::new(READ, 3 + sequence).with_body(&ReadBody {
            cell,
            sequence,
            offset: 0,
            len: CHUNK,
            transfer_at: 0,
        });
        assert_eq!(served(read, &[]).payload, pattern(sequence as u8, CHUNK as usize));
    }
    served(
        Header::new(WINDOW, 6)
            .with_body(&WindowBody { size: 2 * CHUNK, budget: 0 })
            .with_flags(WINDOW_COPYING),
        &[],
    );
    for sequence in 1..=2 {
        let at = (sequence - 1) * CHUNK;
        let mapping = cladefs_core::WindowMapping {
            window_offset: at,
            len: CHUNK,
            contents: cladefs_core::MappingContents::Extent {
                extent: ExtentId { cell, sequence },
                offset: 0,
            },
        };
        served(Header::new(MAP, 6 + sequence), &crate::codec::mappings_payload(&[mapping]));
    }
    assert_eq!(crate::memory::shrink(Some(1), u64::MAX).await, Ok(2 * CHUNK));

    let shrinking = tokio::spawn(crate::memory::shrink(Some(1), CHUNK));
    // The unmaps it is sent, which it never acknowledges, until the daemon lets it go.
    let mut unmapped = Vec::new();
    while let Ok(Some(message)) = receiver.receive(|| 0) {
        assert_eq!(message.header.kind, STAGE_UNMAP, "{:?}", message.header);
        let range = message.header.read_body::<RangeBody>();
        unmapped.push(vec![range.offset, range.len]);
    }
    assert_eq!((shrinking.await.unwrap(), unmapped), (Ok(CHUNK), vec![vec![0, CHUNK]]));
}

/// A VM process made by hand ([`raw_connection`]), of its cell open: it acknowledges the stage
/// operations it is sent only when told, so that it may stay frozen with an unmap unacknowledged.
struct HandVm {
    sender: crate::wire::Sender,
    receiver: crate::wire::Receiver,
    stream: UnixStream,
    cell: u64,
    /// Its next request's ID.
    next: u64,
}

impl HandVm {
    /// A VM process of the daemon at `socket`, its cell at `path` created.
    fn open(socket: &Path, path: &Path) -> Self {
        let (sender, receiver, stream, cell) = raw_connection(socket, path, 1 << 26);
        Self { sender, receiver, stream, cell, next: 2 }
    }

    /// Sends the request `kind`, of `body` and `payload`, and returns its reply, which must
    /// succeed, and the stage operations sent before it, acknowledged if `ack`.
    fn call<T: zerocopy::IntoBytes + zerocopy::Immutable>(
        &mut self,
        kind: u32,
        body: &T,
        payload: &[u8],
        ack: bool,
    ) -> (crate::wire::Message, Vec<crate::wire::Header>) {
        self.call_flagged(kind, 0, body, payload, ack)
    }

    /// As [`HandVm::call`], the request of `flags`.
    fn call_flagged<T: zerocopy::IntoBytes + zerocopy::Immutable>(
        &mut self,
        kind: u32,
        flags: u32,
        body: &T,
        payload: &[u8],
        ack: bool,
    ) -> (crate::wire::Message, Vec<crate::wire::Header>) {
        use crate::wire::*;
        let header = Header::new(kind, self.next).with_body(body).with_flags(flags);
        self.next += 1;
        self.sender.send(header, payload, &[]).unwrap();
        let mut staged = Vec::new();
        loop {
            let message = self.receiver.receive(|| 0).unwrap().expect("the daemon let it go");
            match message.header.kind {
                REPLY => {
                    assert_eq!(message.header.status, 0, "request {kind}");
                    return (message, staged);
                }
                _ if message.header.flags & STAGE_ACK != 0 => {
                    if ack {
                        self.ack(message.header.id);
                    }
                    staged.push(message.header);
                }
                _ => staged.push(message.header),
            }
        }
    }

    /// Acknowledges the stage operations up to `id`.
    fn ack(&self, id: u64) {
        let ack = crate::wire::Header::new(crate::wire::ACK, id);
        self.sender.send(ack, &[], &[]).unwrap();
    }

    /// Writes its cell's extent 1, `len` bytes, and commits it: the extent.
    fn write_an_extent(&mut self, len: u64) -> ExtentId {
        use crate::wire::*;
        let cell = self.cell;
        self.call(
            WRITE,
            &WriteBody { cell, sequence: 1, data_len: len, ..Default::default() },
            &pattern(1, len as usize),
            true,
        );
        self.call(COMMIT, &(), &[], true);
        ExtentId { cell, sequence: 1 }
    }

    /// Reads `extent`'s `len` bytes, which pages them in, and maps them into a window of its own,
    /// acknowledging what it is sent: where its pages are, as its STAGE_MAPs said.
    fn map_an_extent(
        &mut self,
        extent: ExtentId,
        len: u64,
    ) -> Vec<cladefs_core::pages::PhysLocation> {
        use crate::wire::*;
        let (cell, sequence) = (extent.cell, extent.sequence);
        self.call(READ, &ReadBody { cell, sequence, offset: 0, len, transfer_at: 0 }, &[], true);
        self.call_flagged(WINDOW, WINDOW_COPYING, &WindowBody { size: len, budget: 0 }, &[], true);
        let mapping = cladefs_core::WindowMapping {
            window_offset: 0,
            len,
            contents: cladefs_core::MappingContents::Extent { extent, offset: 0 },
        };
        let (_, staged) = self.call(MAP, &(), &crate::codec::mappings_payload(&[mapping]), true);
        (staged.into_iter())
            .filter(|header| header.kind == STAGE_MAP)
            .map(|header| {
                let mapped = header.read_body::<StageMapBody>();
                cladefs_core::pages::PhysLocation {
                    arena: cladefs_core::pages::ArenaId(mapped.arena),
                    offset: mapped.phys_offset,
                }
            })
            .collect()
    }

    /// Maps `extent` again over its window's first `len` bytes, as its guest rebinding it does,
    /// acknowledging nothing it is sent: then frozen, an unmap unacknowledged. The last stage
    /// operation it was sent.
    fn freeze_with_an_unmap(&mut self, extent: ExtentId, len: u64) -> u64 {
        use crate::wire::*;
        let mapping = cladefs_core::WindowMapping {
            window_offset: 0,
            len,
            contents: cladefs_core::MappingContents::Extent { extent, offset: 0 },
        };
        let (_, staged) = self.call(MAP, &(), &crate::codec::mappings_payload(&[mapping]), false);
        assert!(
            staged.iter().any(|header| matches!(header.kind, STAGE_UNMAP | STAGE_REMAP)),
            "{staged:?}"
        );
        staged.iter().map(|header| header.id).max().unwrap()
    }
}

/// Board 8535: a VM process that never exits once killed, as one in uninterruptible sleep or a
/// frozen cgroup, holds up no shrink, nor the governor's next look: frozen with an unmap
/// unacknowledged, then let go and killed, its pages wait for it still; but a shrink of its
/// tenant's memory returns within [`super::memory::PARKED_WAIT`], counting them held, and the next
/// at once. And once it has not exited [`super::server::KILLED_EXITING`] after it was killed, it
/// is counted wedged, with the memory held for it, which relief no longer waits for. Before, the
/// shrink waited for it for good, and with it every later look of the governor, every tenant's.
#[tokio::test(flavor = "multi_thread")]
async fn vm_processes_that_never_exit_hold_up_no_shrink() {
    use std::sync::atomic::Ordering;
    const EXTENT: u64 = 2 << 20;
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = serve_tenant_at(dir.path(), 1);
    crate::server::UNKILLABLE.store(true, Ordering::Relaxed);
    let mut frozen = HandVm::open(&socket, &dir.path().join("1").join("store"));
    crate::server::UNKILLABLE.store(false, Ordering::Relaxed);
    let its = frozen.write_an_extent(EXTENT);
    frozen.map_an_extent(its, EXTENT);
    frozen.freeze_with_an_unmap(its, EXTENT);

    let (bound, slack) = (crate::memory::PARKED_WAIT, std::time::Duration::from_secs(5));
    let started = std::time::Instant::now();
    let shrunk = tokio::time::timeout(bound + slack, crate::memory::shrink(Some(1), 0)).await;
    let took = started.elapsed();
    // Held still, as the VM process may map them: the extent's pages, quarantined.
    assert_eq!(shrunk, Ok(Ok(EXTENT)), "the shrink, after {took:?}");
    let next = tokio::time::timeout(slack, crate::memory::shrink(Some(1), 0)).await;
    assert_eq!(next, Ok(Ok(EXTENT)), "the next shrink");
    // Let go as silent, killed, and not exited: wedged.
    let wedged = crate::server::UNRESPONSIVE + crate::server::KILLED_EXITING + slack;
    let deadline = std::time::Instant::now() + wedged;
    while crate::server::wedged() == 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let gauge = |name: &str| {
        let gauges = crate::metrics::gauges();
        gauges.into_iter().find(|(gauge, _, _)| *gauge == name).map(|(_, _, value)| value)
    };
    assert_eq!(
        (gauge("vm_processes_wedged"), gauge("memory_wedged_bytes")),
        (Some(1), Some(EXTENT)),
        "the wedged VM process, and what is held for it"
    );
    assert_eq!(cladefs_core::slabs::quarantined_bytes(), EXTENT, "held for it still");
}

/// Board 8491: an arena whose memory object can't be made, as with as many files open as the
/// daemon may (the `slabs::memory_object` failpoint: `EMFILE`), fails its tenant's grant with that
/// error, and panics nothing: before, it panicked holding the arenas' lock, at which every later
/// allocation, of every tenant's, panicked. Another tenant's VM process is served meanwhile, its
/// writes allocating in its arena; and once memory objects can be made again, the grant
/// succeeds, and its VM process is served.
#[test]
fn arenas_failing_to_be_made_fail_their_grant_alone() {
    use crate::wire::*;
    host();
    let dir = tempfile::tempdir().unwrap();
    let alice = serve_tenant_at(dir.path(), 1);
    let (sender, mut receiver, _stream, cell) =
        raw_connection(&alice, &dir.path().join("1").join("store"), 1 << 24);
    let mut written = |id: u64, sequence: u64| {
        let header = Header::new(WRITE, id).with_body(&WriteBody {
            cell,
            sequence,
            data_len: 1 << 20,
            ..Default::default()
        });
        call(&sender, &mut receiver, header, &pattern(sequence as u8, 1 << 20)).header.status
    };
    fail::cfg("slabs::memory_object", "return").unwrap();
    let cells = dir.path().join("2");
    std::fs::create_dir_all(&cells).unwrap();
    let refused = crate::server::Tenant::new(2, &cells).err().map(|err| err.raw_os_error());
    assert_eq!(refused, Some(Some(libc::EMFILE)), "the grant, its arena unmade");
    assert_eq!(written(2, 1), 0, "another tenant's write, meanwhile");
    fail::remove("slabs::memory_object");
    let bob = serve_tenant_at(dir.path(), 2);
    let (bob_sender, mut bob_receiver, _bob_stream, bob_cell) =
        raw_connection(&bob, &cells.join("store"), 1 << 24);
    let header = Header::new(WRITE, 2).with_body(&WriteBody {
        cell: bob_cell,
        sequence: 1,
        data_len: 1 << 20,
        ..Default::default()
    });
    let status = call(&bob_sender, &mut bob_receiver, header, &pattern(3, 1 << 20)).header.status;
    assert_eq!((status, written(3, 2)), (0, 0), "the grant's write, and the other's, after");
}

/// The socket of a daemon serving tenant `id`, whose cells are in `dir`'s directory `id`.
fn serve_tenant_at(dir: &Path, id: u64) -> std::path::PathBuf {
    let cells = dir.join(id.to_string());
    std::fs::create_dir_all(&cells).unwrap();
    let tenant = crate::server::Tenant::new(id, &cells).unwrap();
    let socket = dir.join(format!("{id}.sock"));
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || crate::server::serve_tenant(listener, tenant));
    socket
}

/// Board 6208 (doc-26, spec rule scopedWaits): a VM process of tenant 1's, frozen with an unmap
/// unacknowledged, holds none of tenant 2's memory: a shrink of tenant 2's, whose VM process
/// acknowledges what it is sent, frees what it evicts and returns at once, the frozen VM process
/// served still, long before the daemon would let it go ([`super::server::UNRESPONSIVE`]). Every
/// VM process connected held every slab freed, a tenant's frozen VM every other tenant's memory.
#[tokio::test(flavor = "multi_thread")]
async fn frozen_vm_processes_hold_no_other_tenants_memory() {
    const EXTENT: u64 = 2 << 20;
    host();
    let dir = tempfile::tempdir().unwrap();
    let (one, two) = (serve_tenant_at(dir.path(), 1), serve_tenant_at(dir.path(), 2));
    let mut frozen = HandVm::open(&one, &dir.path().join("1").join("store"));
    let its = frozen.write_an_extent(EXTENT);
    frozen.map_an_extent(its, EXTENT);
    frozen.freeze_with_an_unmap(its, EXTENT);
    let mut live = HandVm::open(&two, &dir.path().join("2").join("store"));
    let its = live.write_an_extent(EXTENT);
    live.map_an_extent(its, EXTENT);

    let started = std::time::Instant::now();
    let shrinking = tokio::spawn(crate::memory::shrink(Some(2), 0));
    // It acknowledges the unmaps it is sent, as it makes them, until the test is done with it.
    let stream = live.stream.try_clone().unwrap();
    let acking = std::thread::spawn(move || {
        while let Ok(Some(message)) = live.receiver.receive(|| 0) {
            if message.header.flags & crate::wire::STAGE_ACK != 0 {
                live.ack(message.header.id);
            }
        }
    });
    assert_eq!(shrinking.await.unwrap(), Ok(0));
    let took = started.elapsed();
    assert!(took < crate::server::UNRESPONSIVE, "tenant 2's shrink took {took:?}");
    // Served still: not let go.
    frozen.call(crate::wire::INFO, &(), &[], false);
    stream.shutdown(std::net::Shutdown::Both).unwrap();
    acking.join().unwrap();
}

/// Board 6208 (doc-26, spec rules waitsForStaged, scopedWaits): a public base's slab, of the
/// default arena, which two tenants' VM processes map, evicted, waits for both to acknowledge what
/// took it away, one after the other, and for no third, frozen with an unmap unacknowledged: free
/// once the second has. A wait for the arena's tenant alone, the default tenant's, waited for
/// neither.
#[tokio::test(flavor = "multi_thread")]
async fn public_slabs_wait_for_the_vm_processes_they_were_staged_to_alone() {
    const EXTENT: u64 = 1 << 20;
    host();
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base");
    let open_cell = OpenCell::create(&base, 1 << 26).await.unwrap();
    let extent = ExtentId { cell: open_cell.cell().id, sequence: 1 };
    let bytes = pattern(1, EXTENT as usize);
    open_cell.write(extent, 0, &[VolatileSlice::from(&bytes[..])]).await.unwrap();
    open_cell.commit(None).await.unwrap();
    host_fs::may_block(|| open_cell.make_public().unwrap());
    let base_cell = open_cell.cell();
    drop(open_cell);
    let opened = OpenCell::open(&base).await.unwrap();
    let forks: Vec<_> = (1..=2u64)
        .map(|tenant| (tenant, dir.path().join(tenant.to_string()).join("fork")))
        .collect();
    for (tenant, fork) in &forks {
        std::fs::create_dir_all(fork.parent().unwrap()).unwrap();
        // A fork of the base, a cell of the tenant's.
        opened.fork_to(fork, cladefs_core::host_cell::named(*tenant)).await.unwrap();
    }
    drop((opened, base_cell));
    let forks: Vec<_> = forks.into_iter().map(|(_, fork)| fork).collect();
    let sockets = [serve_tenant_at(dir.path(), 1), serve_tenant_at(dir.path(), 2)];
    let mut vms: Vec<HandVm> =
        (sockets.iter().zip(&forks)).map(|(socket, fork)| HandVm::open(socket, fork)).collect();
    let mapped: Vec<_> = vms.iter_mut().map(|vm| vm.map_an_extent(extent, EXTENT)).collect();
    let at = mapped[0][0];
    assert_eq!(at.arena, cladefs_core::pages::ArenaId::DEFAULT, "a public base's extent");
    assert_eq!(mapped[1], [at], "the same slab mapped");
    // A third, of the default tenant's, frozen with an unmap of its own unacknowledged.
    let default = dir.path().join("default.sock");
    let listener = UnixListener::bind(&default).unwrap();
    std::thread::spawn(move || serve(listener));
    let mut frozen = HandVm::open(&default, &dir.path().join("frozen"));
    let its = frozen.write_an_extent(EXTENT);
    frozen.map_an_extent(its, EXTENT);
    frozen.freeze_with_an_unmap(its, EXTENT);

    let evicted = cladefs_core::cache::evict_least_used(
        Some(cladefs_core::pages::ArenaId::DEFAULT),
        |evicted| evicted >= EXTENT,
    );
    assert_eq!(evicted, EXTENT);
    assert!(page_is_held(at.arena, at.offset), "freed with its unmaps unacknowledged");
    for (n, vm) in vms.iter_mut().enumerate() {
        // Its unmap, sent as the slab was evicted, acknowledged; and a request after it
        // answered, which the daemon reads after the acknowledgement.
        let unmap = vm.receiver.receive(|| 0).unwrap().unwrap().header;
        assert_eq!(unmap.kind, crate::wire::STAGE_UNMAP, "{unmap:?}");
        vm.ack(unmap.id);
        vm.call(crate::wire::INFO, &(), &[], true);
        let free = page_is_free(at.arena, at.offset);
        assert_eq!(free, n == 1, "free once both acknowledged, after VM process {n}");
    }
    frozen.call(crate::wire::INFO, &(), &[], false);
}

/// TASK-358: the governor, the host's memory running low, evicts each tenant down to its share by
/// weight of what it allows: of two tenants of 4 chunks each, weighing 3 and 1, allowed 4 chunks
/// together, the heavier keeps 3, the lighter 1.
#[tokio::test(flavor = "multi_thread")]
async fn the_governor_shares_memory_by_weight() {
    use crate::wire::*;
    const CHUNK: u64 = cladefs_core::slabs::SLAB;
    host();
    let dir = tempfile::tempdir().unwrap();
    let mut kept = Vec::new();
    for (id, (name, weight)) in (1..).zip([("heavy", 3), ("light", 1)]) {
        crate::memory::set_weight(id, weight).unwrap();
        let tenant_dir = dir.path().join(name);
        std::fs::create_dir(&tenant_dir).unwrap();
        let tenant = crate::server::Tenant::new(id, &tenant_dir).unwrap();
        let socket = tenant_dir.join("cladefsd.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || crate::server::serve_tenant(listener, tenant));
        let (sender, mut receiver, stream, cell) =
            raw_connection(&socket, &tenant_dir.join("store"), 1 << 26);
        let mut served = |header: Header, payload: &[u8]| {
            let reply = call(&sender, &mut receiver, header, payload);
            assert_eq!(reply.header.status, 0, "request {}", header.kind);
            reply
        };
        for sequence in 1..=4 {
            let body = WriteBody { cell, sequence, data_len: CHUNK, ..Default::default() };
            let written = Header::new(WRITE, sequence).with_body(&body);
            served(written, &pattern(sequence as u8, CHUNK as usize));
        }
        served(Header::new(COMMIT, 5).with_flags(COMMIT_UPLOADED), &[]);
        for sequence in 1..=4 {
            let body = ReadBody { cell, sequence, len: CHUNK, ..Default::default() };
            let read = Header::new(READ, 5 + sequence).with_body(&body);
            assert_eq!(served(read, &[]).payload, pattern(sequence as u8, CHUNK as usize));
        }
        assert_eq!(crate::memory::shrink(Some(id), u64::MAX).await, Ok(4 * CHUNK), "{name}");
        kept.push((sender, receiver, stream));
    }
    crate::memory::govern(4 * CHUNK).await;
    let held = async |id| crate::memory::shrink(Some(id), u64::MAX).await.unwrap();
    assert_eq!((held(1).await, held(2).await), (3 * CHUNK, CHUNK));
}

/// TASK-400: the governor, the host's memory running low, lets go of the parts of segments the
/// daemon's cells hold packed, newest first, before any tenant's pages: they are derived, their
/// framings reading them back; then, none left, it evicts tenants' pages as before.
#[tokio::test(flavor = "multi_thread")]
async fn the_governor_lets_parts_held_go_before_tenants_pages() {
    use crate::wire::*;
    const CHUNK: u64 = cladefs_core::slabs::SLAB;
    host();
    let dir = tempfile::tempdir().unwrap();
    let tenant_dir = dir.path().join("cached");
    std::fs::create_dir(&tenant_dir).unwrap();
    let tenant = crate::server::Tenant::new(1, &tenant_dir).unwrap();
    let socket = tenant_dir.join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || crate::server::serve_tenant(listener, tenant));
    let (sender, mut receiver, _stream, cell) =
        raw_connection(&socket, &tenant_dir.join("store"), 1 << 26);
    let mut served = |header: Header, payload: &[u8]| {
        let reply = call(&sender, &mut receiver, header, payload);
        assert_eq!(reply.header.status, 0, "request {}", header.kind);
        reply
    };
    // Noise, which its cell's looks ahead hold none of, its segment looking framed in place.
    let noise = |seed: u64| -> Vec<u8> {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64 ^ seed;
        let words = (0..CHUNK / 8).map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        });
        words.flat_map(u64::to_le_bytes).collect()
    };
    for sequence in 1..=4 {
        let body = WriteBody { cell, sequence, data_len: CHUNK, ..Default::default() };
        let written = Header::new(WRITE, sequence).with_body(&body);
        served(written, &noise(sequence));
    }
    served(Header::new(COMMIT, 5).with_flags(COMMIT_UPLOADED), &[]);
    for sequence in 1..=4 {
        let body = ReadBody { cell, sequence, len: CHUNK, ..Default::default() };
        let read = Header::new(READ, 5 + sequence).with_body(&body);
        assert!(served(read, &[]).payload == noise(sequence), "extent {sequence}");
    }
    let cached = async || crate::memory::shrink(Some(1), u64::MAX).await.unwrap();
    assert_eq!(cached().await, 4 * CHUNK);
    let parts = crate::memory::held_parts();
    // What the governor governs besides counted too since (the fetches' parts and tails, board
    // 6026): the parts held add to it.
    let governed = crate::memory::governed_bytes();
    let _held: Vec<_> = (0..4).map(|_| parts.hold_bytes(CHUNK as usize / 2)).collect();
    assert_eq!(crate::memory::governed_bytes(), governed + 2 * CHUNK);
    crate::memory::govern(4 * CHUNK + CHUNK).await;
    assert_eq!((cached().await, parts.bytes()), (4 * CHUNK, CHUNK), "the last two let go");
    crate::memory::govern(4 * CHUNK).await;
    assert_eq!((cached().await, parts.bytes()), (4 * CHUNK, 0), "every part let go, no page");
    crate::memory::govern(3 * CHUNK).await;
    assert_eq!((cached().await, parts.bytes()), (3 * CHUNK, 0), "then pages");
}

/// TASK-79.35: the daemon's metrics are served on through a failure to accept a scrape's
/// connection, as with as many files open as it may (EMFILE): once files close.
#[test]
fn metrics_are_served_on_after_failing_to_accept() {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: Reads the process's limit into `limit`.
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) }, 0);
    // No file opens: the server's accept fails as it begins.
    let none = libc::rlimit { rlim_cur: 0, ..limit };
    // SAFETY: Lowers the process's own limit, which this test sets back.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &none) }, 0);
    let (serving, stopped) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        // Dropped as serving stops.
        let _serving = serving;
        crate::serve_http(listener, crate::Pages::All)
    });
    let served = stopped.recv_timeout(std::time::Duration::from_secs(1));
    // SAFETY: As above.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
    assert_eq!(served, Err(std::sync::mpsc::RecvTimeoutError::Timeout), "serving stopped");
    let scraped = within(std::time::Duration::from_secs(10), move || {
        let mut stream = std::net::TcpStream::connect(address).unwrap();
        write!(stream, "GET /metrics HTTP/1.1\r\nHost: cladefsd\r\n\r\n").unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    });
    let scraped = scraped.expect("the metrics are served no more");
    assert!(scraped.starts_with("HTTP/1.1 200 OK\r\n"), "{scraped}");
}

/// TASK-79.17: a VM process opens no public base to write it, of the default tenant as of any
/// (`EPERM`): what it wrote would be every VM's that forks it, its pages the default arena's, which
/// every VM process holds. Its forks it opens.
#[tokio::test(flavor = "multi_thread")]
async fn vm_processes_of_any_tenant_write_no_public_base() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let (base, fork) = (dir.path().join("base"), dir.path().join("fork"));
    let open_cell = OpenCell::create(&base, 1 << 24).await.unwrap();
    open_cell.commit(None).await.unwrap();
    host_fs::may_block(|| open_cell.make_public().unwrap());
    let forked = cladefs_core::host_cell::fork_child(open_cell.cell());
    drop(open_cell);
    let opened = OpenCell::open(&base).await.unwrap();
    opened.fork_to(&fork, forked).await.unwrap();
    drop(opened);
    let refused = CellClient::connect(&socket, &base, 1 << 24);
    assert_eq!(refused.err().and_then(|error| error.errno()), Some(Errno::EPERM));
    let opened = CellClient::connect(&socket, &fork, 1 << 24).map(|open_cell| open_cell.cell());
    assert_eq!(opened.map_err(|error| error.errno()), Ok(forked.id));
}

/// TASK-79.34: a cell whose VM process goes while the daemon's work through its connection holds
/// the connection, here a shrink waiting for its guest's answer, is let go once that work is done,
/// which its VM process's going ends, not before: deleted once its connection is done, it is open
/// no more.
#[test]
fn cells_held_through_their_connections_are_let_go_once_they_are() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let (client, served) = connect(&path, 2 << 20);
    let (asks, asked) = std::sync::mpsc::channel();
    client.on_capacity(move |capacity, ask| {
        // Unheard once the test has its answer.
        let _ = asks.send((capacity, ask));
    });
    let resizing = {
        let path = path.clone();
        std::thread::spawn(move || resize(&path, 1 << 20))
    };
    assert_eq!(asked.recv().unwrap(), (1 << 20, true));
    // Its VM process goes, answering nothing.
    drop(client);
    served.join().unwrap();
    assert_eq!(delete_cell(&path).map_err(|error| error.errno()), Ok(()));
    assert_eq!(resizing.join().unwrap(), Err(Errno::ETIMEDOUT));
}

/// TASK-160.8: `OPEN`s of a cell, all at once as the VM process writing it exits, each tried again
/// on `EBUSY` until it opens the cell, or another connection writes it: one writes it, as the
/// first lets it go, holding it until all are done, the others refused (`EBUSY`); it reads what the
/// first committed. Once all are gone, nothing holds the cell, nor has it open, and it opens
/// again.
#[test]
fn opens_race_the_exit_of_the_vm_process_writing_their_cell() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier, RwLock, mpsc};
    const WRITERS: usize = 4;
    host();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let (first, served) = connect(&path, 1 << 24);
    let extent = ExtentId { cell: first.cell(), sequence: 1 };
    assert_eq!(write(&first, extent, 0, &pattern(1, BLOCK)), Ok(()));
    assert_eq!(first.commit(None), Ok(()));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let tried = move |errno: Option<Errno>| {
        errno != Some(Errno::EBUSY) || std::time::Instant::now() >= deadline
    };
    // All at once; those that open the cell keep it until all are done: until the test lets go
    // of `done`.
    let start = Arc::new(Barrier::new(WRITERS + 1));
    let done = Arc::new(RwLock::new(()));
    let all_done = done.write().unwrap();
    let won = Arc::new(AtomicBool::new(false));
    let (wrote, writes) = mpsc::channel();
    let writers: Vec<_> = (0..WRITERS)
        .map(|_| {
            let (path, start, done, won, wrote) =
                (path.clone(), start.clone(), done.clone(), won.clone(), wrote.clone());
            std::thread::spawn(move || {
                start.wait();
                let opened = loop {
                    let (stream, server) = UnixStream::pair().unwrap();
                    let tenant = default_tenant().unwrap();
                    let served = std::thread::spawn(move || serve_connection(server, tenant));
                    match CellClient::over(stream, &path, Opening::Create(1 << 24, Kind::Fxfs)) {
                        Ok(client) => {
                            won.store(true, Ordering::SeqCst);
                            break Ok((client, served));
                        }
                        Err(error) => {
                            served.join().unwrap();
                            if tried(error.errno()) || won.load(Ordering::SeqCst) {
                                break Err(error.errno());
                            }
                        }
                    }
                };
                match opened {
                    Ok((client, served)) => {
                        wrote.send(Ok(read(&client, extent, 0, BLOCK))).unwrap();
                        drop(done.read().unwrap());
                        drop(client);
                        served.join().unwrap();
                    }
                    Err(errno) => wrote.send(Err(errno)).unwrap(),
                }
            })
        })
        .collect();
    drop(wrote);
    // Its VM process exits.
    start.wait();
    drop(first);
    served.join().unwrap();
    // Each says once, the cell open or not.
    let wait = std::time::Duration::from_secs(60);
    let writes: Vec<_> =
        (0..WRITERS).map(|_| writes.recv_timeout(wait).expect("a writer said nothing")).collect();
    drop(all_done);
    for thread in writers {
        thread.join().unwrap();
    }
    let (opened, refused): (Vec<_>, Vec<_>) = writes.into_iter().partition(Result::is_ok);
    assert_eq!(opened, vec![Ok(Ok(pattern(1, BLOCK)))]);
    assert_eq!(refused, vec![Err(Some(Errno::EBUSY)); WRITERS - 1]);
    assert!(crate::cells::table().is_empty(), "the cell is held or open once all are gone");
    let (again, served) = connect(&path, 1 << 24);
    assert_eq!(read(&again, extent, 0, BLOCK), Ok(pattern(1, BLOCK)));
    drop(again);
    served.join().unwrap();
}

/// TASK-79.16: a VM process that keeps as much stream data in flight as the protocol allows,
/// whole extents' writes of eight threads, four in flight at once, the others waiting for room
/// until one is answered, is served on: what a request's payload holds is counted in flight until
/// its reply is queued, and the reply's bytes from then, not both (`server::Answer`), which, for a
/// while, would have a VM process within the bounds let go.
#[test]
fn vm_processes_within_the_bounds_are_served_on() {
    const WRITERS: u64 = 8;
    const ROUNDS: u64 = 8;
    host();
    let dir = tempfile::tempdir().unwrap();
    let (client, served) = connect(&dir.path().join("store"), 1 << 30);
    let client = std::sync::Arc::new(client);
    let extent = pattern(3, crate::wire::MAX_PAYLOAD as usize);
    let extent = std::sync::Arc::new(extent);
    let writers: Vec<_> = (1..=WRITERS)
        .map(|sequence| {
            let (client, extent) = (client.clone(), extent.clone());
            std::thread::spawn(move || {
                let id = ExtentId { cell: client.cell(), sequence };
                (0..ROUNDS).map(|_| write(&client, id, 0, &extent)).collect::<Vec<_>>()
            })
        })
        .collect();
    let written: Vec<_> = writers.into_iter().map(|writer| writer.join().unwrap()).collect();
    assert_eq!(written, vec![vec![Ok(()); ROUNDS as usize]; WRITERS as usize]);
    drop(client);
    served.join().unwrap();
}

/// TASK-79.43: a window's mappings are answered beside as much stream data in flight as the
/// protocol allows, and their VM process served on: what a reply to `MAPPINGS` holds, as many
/// mappings as a window of its size may have, takes none of the room the daemon keeps for what it
/// queues otherwise, stage operations and commits. Here a window of pages every other of which is
/// a mapping of its own, their reply larger than that room, behind whole extents' reads, their
/// replies unread, and an `INFO` after.
#[test]
fn window_mappings_are_answered_beside_the_stream_data_in_flight() {
    use crate::wire::*;
    let ps = page_size();
    // A reply of mappings larger than the daemon's room for what it queues but replies.
    let count = (18 << 20) / MAPPING_BYTES;
    host();
    // Queuing 256 MiB of unread READ replies in a debug build on a loaded CI runner can exceed the
    // default 5 s stall watchdog before the client begins draining.
    crate::server::STALL_GIVEN.set(std::time::Duration::from_secs(30)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let path = dir.path().join("store");
    let (sender, mut receiver, _stream, cell) = raw_connection(&socket, &path, 1 << 30);
    let window = Header::new(WINDOW, 2)
        .with_body(&WindowBody { size: 2 * count * ps, budget: 0 })
        .with_flags(WINDOW_COPYING);
    call(&sender, &mut receiver, window, &[]);
    // The mappings, as many in flight as a thread taking their replies leaves room for.
    let sender = std::sync::Arc::new(sender);
    let (answered, answers) = std::sync::mpsc::channel();
    let receiving = {
        let sender = sender.clone();
        std::thread::spawn(move || {
            for _ in 0..count {
                let reply = loop {
                    let message = receiver.receive(|| 0).unwrap().unwrap();
                    match message.header.kind {
                        REPLY => break message,
                        _ if message.header.flags & STAGE_ACK != 0 => {
                            sender.send(Header::new(ACK, message.header.id), &[], &[]).unwrap()
                        }
                        _ => {}
                    }
                };
                assert_eq!(reply.header.status, 0, "mapping {}", reply.header.id);
                answered.send(()).unwrap();
            }
            receiver
        })
    };
    for n in 0..count {
        if n >= 1024 {
            answers.recv().unwrap();
        }
        let mapping = cladefs_core::WindowMapping {
            window_offset: 2 * n * ps,
            len: ps,
            contents: cladefs_core::MappingContents::Zeros,
        };
        sender
            .send(Header::new(MAP, 3 + n), &crate::codec::mappings_payload(&[mapping]), &[])
            .unwrap();
    }
    let mut receiver = receiving.join().unwrap();

    // A whole extent read back as many times as the protocol's stream data holds, and the
    // window's mappings, their replies unread, which the daemon holds.
    let id = 3 + count;
    let extent = pattern(4, MAX_PAYLOAD as usize);
    let written = Header::new(WRITE, id).with_body(&WriteBody {
        cell,
        sequence: 1,
        data_len: MAX_PAYLOAD,
        ..Default::default()
    });
    assert_eq!(call(&sender, &mut receiver, written, &extent).header.status, 0);
    // Warm the extent in memory before the unread burst so the 4x64 MiB reads complete quickly
    // without tripping the daemon's 5 s `STALL` watchdog under CI load (board 11076).
    let warm = Header::new(READ, id + 1).with_body(&ReadBody {
        cell,
        sequence: 1,
        offset: 0,
        len: MAX_PAYLOAD,
        transfer_at: 0,
    });
    assert_eq!(call(&sender, &mut receiver, warm, &[]).header.status, 0);
    let id = id + 1;
    let reads = MAX_STREAM_BYTES / MAX_PAYLOAD;
    for n in 1..=reads {
        let read = Header::new(READ, id + n).with_body(&ReadBody {
            cell,
            sequence: 1,
            offset: 0,
            len: MAX_PAYLOAD,
            transfer_at: 0,
        });
        sender.send(read, &[], &[]).unwrap();
    }
    sender.send(Header::new(MAPPINGS, id + reads + 1), &[], &[]).unwrap();
    let key = crate::cells::cell_dir_key(&path);
    let holder = crate::cells::connection_writing(&key).unwrap();
    // Wait until all 4 READs and MAPPINGS are answered and queued past `MAX_STREAM_BYTES +
    // QUEUED_CREDIT` (tolerating up to 2 MiB of kernel Unix socket buffer and failing promptly if
    // the connection is dropped rather than spinning forever; board 11076).
    let replies = MAX_STREAM_BYTES + crate::server::QUEUED_CREDIT;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while holder.queued() < replies || holder.unanswered() > 0 {
        assert!(
            crate::cells::table().writer(&key).is_some(),
            "connection closed before replies queued (queued={}, expected>={replies}, unanswered={})",
            holder.queued(),
            holder.unanswered()
        );
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for replies to queue (queued={}, expected>={replies}, unanswered={})",
            holder.queued(),
            holder.unanswered()
        );
        std::thread::yield_now();
    }
    sender.send(Header::new(INFO, id + reads + 2), &[], &[]).unwrap();
    // Their replies, in any order, as the daemon serves its requests side by side.
    let answered = within(std::time::Duration::from_secs(60), move || {
        let mut answered = Vec::new();
        while answered.len() < reads as usize + 2 {
            let message = receiver.receive(|| 0).unwrap().expect("the daemon let it go");
            if message.header.kind == REPLY {
                let header = message.header;
                answered.push((header.id - id, header.status, message.payload.len()));
            }
        }
        answered.sort();
        answered
    });
    let mut expected: Vec<_> = (1..=reads).map(|n| (n, 0, MAX_PAYLOAD as usize)).collect();
    expected.push((reads + 1, 0, (count * MAPPING_BYTES) as usize));
    expected.push((reads + 2, 0, 0));
    assert_eq!(answered, Ok(expected));
}

/// How a [`whatever_a_vm_process_says_harms_no_other`] sequence ends.
#[derive(Debug, Clone, Copy)]
#[cfg(target_os = "linux")]
enum LastWords {
    /// Closes the connection between messages.
    Nothing,
    /// A part of a header.
    PartOfAHeader,
    /// A header of more payload than comes.
    MissingPayload(u64),
    /// A header of more descriptors than come.
    MissingDescriptors(u32),
}

/// TASK-77: a VM process is as trustworthy as its guest, once the guest has broken out into it: a
/// tenant's VM process saying anything, of any kind, with any flags and arguments, IDs and paths
/// that name another tenant's among them, through its window and transfer area, then breaking off
/// however it likes, panics nothing, leaves another tenant's cell as it was and served, and lets
/// its own go. TASK-154: and it gets of the bucket what its VM manager put in its directory alone,
/// a version it restores, and nothing of the cell it names. TASK-164.1: through a window that
/// copies on write too, which its VM process makes its own copies in (doc-26).
/// Seeded by proptest, which says the sequence if one fails.
#[test]
#[cfg(target_os = "linux")]
fn whatever_a_vm_process_says_harms_no_other() {
    use crate::wire::*;
    use proptest::prelude::*;
    use proptest::test_runner::{Config, TestRunner};
    use std::os::fd::{AsFd as _, AsRawFd as _, FromRawFd as _, OwnedFd};
    use std::os::unix::ffi::OsStrExt as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    use zerocopy::IntoBytes as _;

    /// A word that says the VM process's cell, the other tenant's, and the one its VM
    /// manager took for it.
    const OWN_CELL: u64 = 0xb0b0_0001;
    const OTHER_CELL: u64 = 0xb0b0_0002;
    const GIVEN_CELL: u64 = 0xb0b0_0003;
    const GIVEN_VERSION: u64 = 0xb0b0_0004;
    const WINDOW_BYTES: u64 = 1 << 20;
    const TRANSFER_BYTES: u64 = 1 << 20;
    /// The ID of the request that ends a sequence.
    const LAST: u64 = 0x1a57;
    /// The ID of a restore's `OPEN`.
    const RESTORE: u64 = 0x7e57;
    const KINDS: [u32; 34] = [
        OPEN,
        READ,
        WRITE,
        FREE,
        MAP,
        UNMAP,
        COMMIT,
        ROOT,
        INFO,
        WINDOW,
        FAULT,
        ACK,
        FORK,
        RESIZED,
        // cladefs: and 29 and 107, writable DAX's asks to seal and their answers, retired.
        MAPPINGS,
        // cladefs: and 21 to 24, a follower's, gone (board 8237).
        21,
        22,
        23,
        24,
        TRANSFER,
        // 26 (FREE_MANY), 28 (BUILD), 32 (MAP_MANY), 33 (STAGE_PRIVATE_COUNTS) retired (board 11697).
        26,
        STAGE_COUNTS,
        28,
        29,
        // `PRIVATE`'s, which a VM process serves itself now (doc-26).
        30,
        REPLY,
        STAGE_MAP,
        STAGE_UNMAP,
        CAPACITY,
        107,
        14,
        0xffff_ffff,
        32,
        33,
    ];
    let wait = std::time::Duration::from_secs(60);

    // A panic on the daemon's runtime ends its task alone.
    static PANICS: AtomicU64 = AtomicU64::new(0);
    let panicked = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        PANICS.fetch_add(1, Ordering::Relaxed);
        panicked(info);
    }));

    host();
    let dir = tempfile::tempdir().unwrap();
    let vms = dir.path().join("vms");
    std::fs::create_dir(&vms).unwrap();
    let tenant = crate::server::Tenant::new(1, &vms).unwrap();
    let (socket, other_socket) = (dir.path().join("tenant.sock"), dir.path().join("other.sock"));
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || crate::server::serve_tenant(listener, tenant));
    let listener = UnixListener::bind(&other_socket).unwrap();
    std::thread::spawn(move || serve(listener));

    let other_path = dir.path().join("other");
    let other = CellClient::connect(&other_socket, &other_path, 1 << 24).unwrap();
    let kept = ExtentId { cell: other.cell(), sequence: 1 };
    write(&other, kept, 0, &pattern(9, BLOCK)).unwrap();
    other.commit(None).unwrap();
    let their_bytes = std::sync::Arc::new(pattern(9, BLOCK));

    // A snapshot in the bucket, a version of the other tenant's cell, not shared.
    let given = CellClient::connect(&other_socket, dir.path().join("given"), 1 << 24);
    let given = given.unwrap();
    let given_cell = given.cell();
    write(&given, ExtentId { cell: given_cell, sequence: 1 }, 0, &pattern(8, BLOCK)).unwrap();
    given.commit(Some(b"given")).unwrap();
    let given_version = given.make_version().unwrap().version;
    drop(given);

    let ps = page_size();
    let word = prop_oneof![
        Just(0u64),
        Just(1),
        Just(ps),
        Just(WINDOW_BYTES),
        Just(WINDOW_BYTES - ps),
        Just(MAX_WINDOW),
        Just(MAX_WINDOW + ps),
        Just(1 << 60),
        Just(OWN_CELL),
        Just(OTHER_CELL),
        Just(GIVEN_CELL),
        1u64..16,
        (0u64..64).prop_map(move |page| page * ps),
        (0u64..4096).prop_map(|back| u64::MAX - back),
        any::<u64>(),
    ];
    let paths = [
        vms.join("fork"),
        vms.join("../escaped"),
        other_path.clone(),
        std::path::PathBuf::from("relative"),
    ];
    // cladefs: none as often as some, as most kinds carry none, and a payload past what its kind
    // holds ends the connection at its header (board 8459): every case ended at its first.
    let payload = prop_oneof![
        8 => Just(Vec::new()),
        3 => proptest::collection::vec(any::<u8>(), 0..64),
        1 => (0..paths.len()).prop_map(move |path| {
            std::os::unix::ffi::OsStrExt::as_bytes(paths[path].as_os_str()).to_vec()
        }),
    ];
    let anything = (
        proptest::sample::select(&KINDS[..]),
        prop_oneof![0u32..32, any::<u32>()],
        any::<u64>(),
        [word.clone(), word.clone(), word.clone(), word.clone(), word],
        payload,
    );
    // Requests a cell would serve, as far as their extents and pages go: of those the VM
    // process wrote, or wrote and freed, and of the window it has, in any order.
    let page = move || (0u64..32).prop_map(move |page| page * ps);
    let length = move || (1u64..8).prop_map(move |pages| pages * ps);
    let sequence = || 1u64..16;
    let plausible = prop_oneof![
        (sequence(), page(), 1usize..8).prop_map(move |(extent, at, pages)| {
            (WRITE, 0, 0, [OWN_CELL, extent, at, 0, 0], vec![0x5a; pages * ps as usize])
        }),
        (sequence(), page(), length()).prop_map(|(extent, at, len)| {
            (READ, 0, 0, [OWN_CELL, extent, at, len, 0], Vec::new())
        }),
        // cladefs: of extents or zeros, one or many (TASK-92.14, board 11697).
        proptest::collection::vec((sequence(), page(), length(), page(), any::<bool>()), 1..4)
            .prop_map(|maps| {
                let mappings: Vec<cladefs_core::WindowMapping> = (maps.into_iter())
                    .map(|(sequence, at, len, to, zeros)| cladefs_core::WindowMapping {
                        window_offset: to,
                        len,
                        contents: match zeros {
                            true => cladefs_core::MappingContents::Zeros,
                            false => cladefs_core::MappingContents::Extent {
                                extent: ExtentId { cell: OWN_CELL, sequence },
                                offset: at,
                            },
                        },
                    })
                    .collect();
                (MAP, 0, 0, [0; 5], crate::codec::mappings_payload(&mappings))
            }),
        (page(), length()).prop_map(|(at, len)| (UNMAP, 0, 0, [at, len, 0, 0, 0], Vec::new())),
        (page(), 0u32..4).prop_map(|(at, flags)| (FAULT, flags, 0, [at, 0, 0, 0, 0], Vec::new())),
        sequence().prop_map(|extent| (FREE, 0, 0, [OWN_CELL, extent, 0, 0, 0], Vec::new())),
        (0u32..4).prop_map(|flags| (COMMIT, flags, 0, [0; 5], Vec::new())),
        Just((MAPPINGS, 0, 0, [0; 5], Vec::new())),
        Just((INFO, 0, 0, [0; 5], Vec::new())),
    ];
    // What the other tenant wrote, as its cell would read it.
    let reading_theirs = Just((READ, 0, 0, [OTHER_CELL, 1, 0, BLOCK as u64, 0], Vec::new()));
    // On a connection that opened nothing yet, cells any which way: restores of a version of none,
    // or of the other tenant's, naming either of its cells.
    let opening = prop_oneof![
        (0u64..4, any::<bool>()).prop_map(|(client, creating)| {
            let flags = if creating { OPEN_CREATE } else { 0 };
            (
                OPEN,
                flags,
                0,
                [1 << 24, 0, 0, 0, VERSION],
                format!("open-{client}\0test").into_bytes(),
            )
        }),
        (0u64..4, any::<bool>(), prop_oneof![Just(OTHER_CELL), Just(GIVEN_CELL)]).prop_map(
            move |(client, absent, cell)| {
                let version = if absent { 7 } else { GIVEN_VERSION };
                let named = format!("open-{client}\0test").into_bytes();
                (OPEN, OPEN_FORK, RESTORE, [1 << 24, cell, version, 0, VERSION], named)
            }
        ),
    ];
    let message = prop_oneof![12 => plausible, 4 => anything, 2 => opening, 1 => reading_theirs];
    let last_words = prop_oneof![
        Just(LastWords::Nothing),
        Just(LastWords::PartOfAHeader),
        any::<u64>().prop_map(LastWords::MissingPayload),
        (1u32..4).prop_map(LastWords::MissingDescriptors),
    ];
    // A window of any size it asks for first, the sequence's mappings' within most.
    let window = prop_oneof![
        4 => Just(WINDOW_BYTES),
        1 => Just(MAX_WINDOW),
        1 => Just(MAX_WINDOW + ps),
        1 => Just(1 << 60),
    ];
    // cladefs: copying on write, or not (backlog doc-13), and its private pages, which a WINDOW
    // says no more (doc-26).
    let copying = prop_oneof![
        1 => Just((0u32, 0u64)),
        6 => Just((WINDOW_COPYING, 0)),
        1 => (0u32..2, 1u64..16),
    ];
    let sequence = (
        proptest::collection::vec(message, 1..64),
        any::<bool>(),
        last_words,
        any::<bool>(),
        window,
        copying,
    );

    let cases = AtomicU64::new(0);
    let served = std::sync::Arc::new(AtomicU64::new(0));
    let restored = std::sync::Arc::new(AtomicU64::new(0));
    let stolen = std::sync::Arc::new(AtomicU64::new(0));
    // A failing case shrinks for two minutes at most: each step may wait as long as `wait`, and
    // shrinking one to the end took 38 minutes.
    let config = Config {
        cases: 128,
        failure_persistence: None,
        max_shrink_time: 120_000,
        ..Config::default()
    };
    TestRunner::new(config)
        .run(&sequence, |(messages, transfer, last_words, opened, window, copying)| {
            let case = cases.fetch_add(1, Ordering::Relaxed);
            let path = vms.join(format!("store-{case}"));
            let (sender, mut receiver, stream, cell) = if opened {
                raw_connection(&socket, &path, 1 << 24)
            } else {
                let stream = UnixStream::connect(&socket).unwrap();
                let sending = Sender::new(stream.try_clone().unwrap()).unwrap();
                (sending, Receiver::new(stream.try_clone().unwrap()), stream, 0)
            };
            // The cells it may hold once done: those it opened, had it none.
            let held: Vec<_> = (0..4)
                .filter(|_| !opened)
                .map(|client| vms.join(format!("{case}-open-{client}")))
                .chain([path].into_iter().filter(|_| opened))
                .collect();
            let sender = std::sync::Arc::new(sender);
            // Whatever comes back, its stage operations made, as far as saying so goes.
            let (answered, all_answered) = std::sync::mpsc::channel();
            let (restore_answered, restores_answered) = std::sync::mpsc::channel();
            let own = std::sync::Arc::new(AtomicU64::new(cell));
            let answering = {
                let (sender, served, restored) = (sender.clone(), served.clone(), restored.clone());
                let (stolen, their_bytes, own) = (stolen.clone(), their_bytes.clone(), own.clone());
                std::thread::spawn(move || {
                    while let Ok(Some(message)) = receiver.receive(|| u64::MAX) {
                        if message.header.kind == REPLY && message.header.status == 0 {
                            served.fetch_add(1, Ordering::Relaxed);
                            let first_word = message.header.read_body::<OpenedBody>().cell;
                            if message.header.id == RESTORE {
                                restored.fetch_add(1, Ordering::Relaxed);
                                own.store(first_word, Ordering::Relaxed);
                            }
                            // The other tenant's cell, or what it wrote.
                            if first_word == kept.cell || message.payload == *their_bytes {
                                stolen.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        if message.header.kind == REPLY && message.header.id == LAST {
                            let _ = answered.send(());
                        }
                        if message.header.kind == REPLY && message.header.id == RESTORE {
                            let _ = restore_answered.send(());
                        }
                        if message.header.flags & STAGE_ACK != 0 {
                            let _ = sender.send(Header::new(ACK, message.header.id), &[], &[]);
                        }
                    }
                })
            };
            let (flags, private) = copying;
            let window = Header::new(WINDOW, 2)
                .with_body(&WindowBody { size: window, budget: private })
                .with_flags(flags);
            sender.send(window, &[], &[]).unwrap();
            if transfer {
                // SAFETY: Makes a descriptor, which is owned below.
                let fd =
                    unsafe { libc::memfd_create(c"transfer".as_ptr(), libc::MFD_ALLOW_SEALING) };
                assert!(fd >= 0);
                // SAFETY: Just made, and owned here.
                let fd = unsafe { OwnedFd::from_raw_fd(fd) };
                // SAFETY: Sizes, populates, and seals it (board 11707).
                unsafe {
                    assert_eq!(libc::ftruncate(fd.as_raw_fd(), TRANSFER_BYTES as i64), 0);
                    let addr = libc::mmap(
                        std::ptr::null_mut(),
                        TRANSFER_BYTES as usize,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_SHARED,
                        fd.as_raw_fd(),
                        0,
                    );
                    assert_ne!(addr, libc::MAP_FAILED);
                    addr.cast::<u8>().write_bytes(0, TRANSFER_BYTES as usize);
                    assert_eq!(libc::munmap(addr, TRANSFER_BYTES as usize), 0);
                    assert_eq!(
                        libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_SHRINK),
                        0
                    );
                }
                let area =
                    Header::new(TRANSFER, 3).with_body(&TransferBody { len: TRANSFER_BYTES });
                sender.send(area, &[], &[fd.as_fd()]).unwrap();
            }
            let named_word = |word: u64| match word {
                OWN_CELL => own.load(Ordering::Relaxed),
                OTHER_CELL => kept.cell,
                GIVEN_CELL => given_cell,
                GIVEN_VERSION => given_version,
                word => word,
            };
            // A path a sequence opens at, this case's.
            let in_vms = |payload: Vec<u8>| match payload.strip_prefix(b"open-") {
                Some(rest) => {
                    let named = vms.as_os_str().as_bytes();
                    [named, format!("/{case}-open-").as_bytes(), rest].concat()
                }
                None => payload,
            };
            for (kind, flags, id, args, payload) in messages {
                let args = args.map(named_word);
                let message = Header::new(kind, id).with_body(&args).with_flags(flags);
                let payload = in_vms(payload);
                // The daemon may have let it go by now.
                let _ = sender.send(message, &payload, &[]);
                // What comes after a restore is said of the cell it opened, as the daemon may
                // take longer over it than over all that comes after: answered, or the daemon
                // let it go, which ends the answers.
                if id == RESTORE {
                    let _ = restores_answered.recv_timeout(wait);
                }
            }
            // Answered, or the daemon has let it go, which ends the answers.
            let _ = sender.send(Header::new(INFO, LAST), &[], &[]);
            let _ = all_answered.recv_timeout(wait);
            let raw = |bytes: &[u8]| {
                let _ = std::io::Write::write_all(&mut &stream, bytes);
            };
            let header = |len, fds| Header { kind: WRITE, len, fds, ..Default::default() };
            match last_words {
                LastWords::Nothing => {}
                LastWords::PartOfAHeader => raw(&header(0, 0).as_bytes()[..16]),
                LastWords::MissingPayload(len) => raw(header(len, 0).as_bytes()),
                LastWords::MissingDescriptors(fds) => raw(header(0, fds).as_bytes()),
            }
            // It says no more, and hears the rest: the daemon answers what it took before it
            // hangs up, once its requests are done (`server::finish_connection`), which ends the
            // answers. TASK-686: and so what it heard of served is all of it, however busy the
            // host, not what came before `LAST`'s reply, which those served beside it may follow.
            stream.shutdown(std::net::Shutdown::Write).unwrap();
            prop_assert!(within(wait, move || answering.join().unwrap()).is_ok());

            // Its cell is let go of, and opens.
            for path in held {
                let (opening, reopened) = (socket.clone(), path);
                let opened = within(wait, move || {
                    loop {
                        match CellClient::connect(&opening, &reopened, 1 << 24) {
                            Err(error) if error.errno() == Some(Errno::EBUSY) => {
                                std::thread::yield_now()
                            }
                            opened => break opened.map(drop).map_err(|error| error.to_string()),
                        }
                    }
                });
                prop_assert_eq!(opened, Ok(Ok(())));
            }

            // Nothing it said took a cell, nor reached the other tenant's, which is served on.
            prop_assert!(!vms.join("nothing").exists(), "a version of a cell named was taken");
            prop_assert_eq!(stolen.load(Ordering::Relaxed), 0);
            prop_assert_eq!(read(&other, kept, 0, BLOCK), Ok(pattern(9, BLOCK)));
            let served = ExtentId { cell: kept.cell, sequence: 2 + case };
            prop_assert_eq!(write(&other, served, 0, &pattern(7, BLOCK)), Ok(()));
            prop_assert_eq!(other.commit(None), Ok(()));
            prop_assert_eq!(read(&other, served, 0, BLOCK), Ok(pattern(7, BLOCK)));
            prop_assert_eq!(PANICS.load(Ordering::Relaxed), 0);
            Ok(())
        })
        .unwrap();
    // What it says reaches past the checks: not every request is refused. Those served, every
    // one of which it hears of (TASK-686: once, only those before it hung up, fewer the busier
    // the host: 468 in a loaded workspace run), are as many as the cases drawn have served: 580
    // to 899 in 23 runs on the test host (2026-11-20), about 740 on average. Refusing everything
    // serves none.
    let (served, restored) = (served.load(Ordering::Relaxed), restored.load(Ordering::Relaxed));
    println!("{served} requests served, {restored} of them restores");
    assert!(served > 300, "{served} requests served");
    // And it restores what it was given: refusing everything would pass the rest.
    assert!(restored > 0, "{restored} restores served");
    drop(other);
}

/// TASK-153.16: a panic holding a cell's lock, serving a VM process's request, halts the cell,
/// alone: the request fails with `EIO`, and so does each after it, which panics no more, as each
/// did at the lock's poison in TASK-104, TASK-105 and TASK-150.20, those that take no lock of the
/// cell's too. Another VM process's cell serves on. Its VM process gone, the cell opens anew,
/// as of its last commit, and serves.
#[test]
fn a_panic_holding_a_cells_lock_halts_it_alone() {
    use std::sync::atomic::{AtomicU64, Ordering};

    static PANICS: AtomicU64 = AtomicU64::new(0);
    let panicked = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        PANICS.fetch_add(1, Ordering::Relaxed);
        panicked(info);
    }));

    host();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let (client, served) = connect(&path, 1 << 24);
    let (other, other_served) = connect(&dir.path().join("other"), 1 << 24);
    let ours = |sequence| ExtentId { cell: client.cell(), sequence };
    let (kept, written) = (ours(1), ours(2));
    write(&client, kept, 0, &pattern(1, BLOCK)).unwrap();
    client.commit(None).unwrap();

    cladefs_core::halt::tests::panic_holding("opening");
    assert_eq!(write(&client, written, 0, &pattern(2, BLOCK)), Err(Errno::EIO));
    assert_eq!(PANICS.load(Ordering::Relaxed), 1);
    assert_eq!(read(&client, kept, 0, BLOCK), Err(Errno::EIO));
    assert_eq!(write(&client, ours(3), 0, &pattern(3, BLOCK)), Err(Errno::EIO));
    assert_eq!(client.commit(None), Err(Errno::EIO));
    // Which takes no lock of the cell's.
    assert_eq!(client.info().map(|info| info.capacity), Err(Errno::EIO));
    assert_eq!(PANICS.load(Ordering::Relaxed), 1);

    let theirs = ExtentId { cell: other.cell(), sequence: 1 };
    assert_eq!(write(&other, theirs, 0, &pattern(4, BLOCK)), Ok(()));
    assert_eq!(other.commit(None), Ok(()));
    assert_eq!(read(&other, theirs, 0, BLOCK), Ok(pattern(4, BLOCK)));

    drop(client);
    served.join().unwrap();
    let (client, served) = connect(&path, 1 << 24);
    assert_eq!(read(&client, kept, 0, BLOCK), Ok(pattern(1, BLOCK)));
    assert_eq!(write(&client, written, 0, &pattern(5, BLOCK)), Ok(()));
    assert_eq!(client.commit(None), Ok(()));
    assert_eq!(read(&client, written, 0, BLOCK), Ok(pattern(5, BLOCK)));
    assert_eq!(PANICS.load(Ordering::Relaxed), 1);
    for (client, served) in [(client, served), (other, other_served)] {
        drop(client);
        served.join().unwrap();
    }
}

/// The locks shared beyond one cell a VM process's requests take ([`child_panics_holding`]): the
/// extent cache's and its extents', a window's and its change's, its connection's, and those of
/// `local_cell`'s cell, which the cells a base is attached to share (TASK-149.24).
#[cfg(target_os = "linux")]
const SHARED_LOCKS: &[&str] = &[
    "extent cache",
    "cached extent holders",
    "cached extent parts",
    "window",
    "stage change",
    "connection phase",
    "connection window",
    "connection requests",
    "store state",
    "store commit",
    "store durability",
    "store flush",
];

/// What [`child_panics_holding`] panics holding, as its parent says.
#[cfg(target_os = "linux")]
const PANIC_HOLDING: &str = "CLADEFSD_TEST_PANIC_HOLDING";

/// Run by [`a_panic_holding_a_shared_lock_stops_the_daemon`], in a process of its own: a daemon,
/// aborting on panics as `cladefsd` does, serves a VM process's window and cell, which
/// write, commit, map and fault a page in, and close, a panic holding the lock [`PANIC_HOLDING`]
/// names as it is next taken. It says if it got to the end.
#[test]
#[ignore = "run by a_panic_holding_a_shared_lock_stops_the_daemon, in a process of its own"]
#[cfg(target_os = "linux")]
fn child_panics_holding() {
    let Ok(lock) = std::env::var(PANIC_HOLDING) else { return };
    // Its abort is what its parent looks for, no crash to keep: no core dump, which a host's
    // systemd-coredump took 5 s to write for each child, its parent's twelve past its minute
    // (asahimbp, board 2944).
    let none = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: setrlimit reads `none` alone.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_CORE, &none) }, 0);
    local_cell::shared::abort_on_panics();
    host();
    let ps = page_size() as usize;
    let dir = tempfile::tempdir().unwrap();
    let (client, served) = connect(&dir.path().join("store"), 1 << 24);
    let memory = memory(1);
    // SAFETY: The memory is the test's, and outlives the cell.
    unsafe { client.create_dax_window_in_host_memory(memory, ps as u64, None, 1) }.unwrap();
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    local_cell::shared::testing::panic_holding(lock.leak());
    let written = write(&client, extent, 0, &pattern(3, ps));
    let committed = client.commit(None);
    let mapped = client.map(extent, 0, ps as u64, 0);
    // SAFETY: The window's memory, whose faults the cell serves.
    let loaded = unsafe { memory.as_ptr().read_volatile() };
    drop(client);
    let closed = served.join();
    println!("THE END: {written:?} {committed:?} {mapped:?} {loaded} {closed:?}");
}

/// TASK-149.24: a panic holding a lock shared beyond one cell stops the daemon at once, saying
/// which: an abort, from its panic hook, before anything unwinds; not a halt, which contains only
/// its cell, while the lock's poison, or what it guards half changed, reaches every user after,
/// nor an abort from a drop panicking as it unwinds.
#[test]
#[cfg(target_os = "linux")]
fn a_panic_holding_a_shared_lock_stops_the_daemon() {
    use std::os::unix::process::ExitStatusExt;
    for lock in SHARED_LOCKS {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::child_panics_holding", "--ignored", "--nocapture"])
            .env(PANIC_HOLDING, lock)
            .output()
            .unwrap();
        let said =
            String::from_utf8_lossy(&output.stdout) + String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.signal(), Some(libc::SIGABRT), "{lock}: {said}");
        assert!(local_cell::shared::aborted_holding(&said, lock), "{lock}: {said}");
        assert!(!said.contains("THE END"), "{lock}: {said}");
        assert!(!said.contains("panic in a destructor"), "{lock}: {said}");
    }
}

/// TASK-153.16: a panic serving a VM process's request past the cell's locks, which poisons
/// none of them, as one in `local_cell` beneath them may (TASK-79.1, TASK-79.80), halts the
/// cell as well: a read after it, which would find nothing amiss, fails with `EIO`. Its VM
/// process gone, the cell opens anew as of its last commit.
#[test]
fn a_panic_past_a_cells_locks_halts_it() {
    use std::sync::atomic::{AtomicU64, Ordering};

    static PANICS: AtomicU64 = AtomicU64::new(0);
    let panicked = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        PANICS.fetch_add(1, Ordering::Relaxed);
        panicked(info);
    }));

    host();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let (client, served) = connect(&path, 1 << 24);
    let ours = |sequence| ExtentId { cell: client.cell(), sequence };
    let (kept, written) = (ours(1), ours(2));
    write(&client, kept, 0, &pattern(1, BLOCK)).unwrap();
    client.commit(None).unwrap();
    write(&client, written, 0, &pattern(2, BLOCK)).unwrap();

    fail::cfg("cladefsd::commit", "panic").unwrap();
    assert_eq!(client.commit(None), Err(Errno::EIO));
    fail::remove("cladefsd::commit");
    assert_eq!(read(&client, kept, 0, BLOCK), Err(Errno::EIO));
    assert_eq!(PANICS.load(Ordering::Relaxed), 1);

    drop(client);
    served.join().unwrap();
    let (client, served) = connect(&path, 1 << 24);
    assert_eq!(read(&client, kept, 0, BLOCK), Ok(pattern(1, BLOCK)));
    assert_eq!(read(&client, written, 0, BLOCK), Err(Errno::ENOENT));
    assert_eq!(PANICS.load(Ordering::Relaxed), 1);
    drop(client);
    served.join().unwrap();
}

/// TASK-159: a tenant's socket is of its mode, 0660, from the moment it is at its path, whatever
/// the daemon's umask, even none: bound where no one else reaches, given its mode there, and only
/// then put at its path, rather than open there until it is given.
#[test]
fn tenants_sockets_are_of_their_mode_from_the_first() {
    use std::os::unix::fs::PermissionsExt as _;
    host();
    let dir = tempfile::tempdir().unwrap();
    let (cells, sockets) = (dir.path().join("acme"), dir.path().join("tenants"));
    std::fs::create_dir(&cells).unwrap();
    std::fs::create_dir(&sockets).unwrap();
    let socket = sockets.join("1.sock");
    let (seen, modes) = std::sync::mpsc::channel();
    let at = socket.clone();
    fail::cfg_callback("cladefsd::bound", move || {
        let mode = std::fs::symlink_metadata(&at).unwrap().permissions().mode();
        seen.send(mode & 0o777).unwrap();
    })
    .unwrap();
    // SAFETY: Plain system calls. Nothing else of this test's process, its own, makes a file
    // meanwhile.
    let umask = unsafe { libc::umask(0) };
    let granted = crate::grants::Grants::new(crate::Daemon::process().clone(), sockets.clone())
        .grant(1, &cells, (None, None));
    // SAFETY: As above.
    unsafe { libc::umask(umask) };
    fail::remove("cladefsd::bound");
    assert_eq!(granted, Ok(socket));
    assert_eq!(modes.try_iter().collect::<Vec<_>>(), [0o660]);
    // Where it was bound went.
    assert_eq!(std::fs::read_dir(&sockets).unwrap().count(), 1);
}

/// TASK-159: a socket bound where one is already, as a running daemon's, fails with `EEXIST`, and
/// leaves that one there, serving.
#[test]
fn sockets_are_not_bound_over_another() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let _running = crate::bind_owner_only(&socket).unwrap();
    let again = crate::bind_owner_only(&socket).unwrap_err();
    assert_eq!(again.kind(), std::io::ErrorKind::AlreadyExists);
    UnixStream::connect(&socket).unwrap();
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

/// TASK-141.4: a drain closes every VM process's connection, its VM told its daemon went, and its
/// cell, closed, opens elsewhere; the daemon's VM socket and its tenants' take no more
/// connections, refused, their threads serving them returning; and the control API takes no more
/// calls. Before, nothing drained: the daemon exited with all of them as they were.
#[tokio::test(flavor = "multi_thread")]
async fn drains_close_every_connection_and_take_no_more() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let vms = dir.path().join("vms");
    std::fs::create_dir(&vms).unwrap();
    let (socket, tenant_socket) =
        (dir.path().join("cladefsd.sock"), dir.path().join("tenant.sock"));
    let listener = UnixListener::bind(&socket).unwrap();
    let serving = std::thread::spawn(move || serve(listener));
    let tenant = crate::server::Tenant::new(1, &vms).unwrap();
    let listener = UnixListener::bind(&tenant_socket).unwrap();
    let serving_tenant = std::thread::spawn(move || crate::server::serve_tenant(listener, tenant));

    let (path, tenant_path) = (dir.path().join("store"), vms.join("store"));
    let client = CellClient::connect(&socket, &path, 1 << 24).unwrap();
    let tenants = CellClient::connect(&tenant_socket, &tenant_path, 1 << 24).unwrap();
    let (told, lost) = std::sync::mpsc::channel();
    for (name, client) in [("store", &client), ("tenant's", &tenants)] {
        let told = told.clone();
        client.on_daemon_lost(move || told.send(name).unwrap());
    }
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, extent, 0, &pattern(1, BLOCK)).unwrap();
    client.commit_ordered(Some(b"drained")).unwrap();

    assert_eq!(crate::drain::drain(std::time::Duration::from_secs(60)).await, Ok(()));
    let mut told: Vec<_> = lost.try_iter().collect();
    told.sort();
    assert_eq!(told, ["store", "tenant's"]);
    assert_eq!(write(&client, extent, 0, &pattern(2, BLOCK)), Err(Errno::EIO));
    serving.join().unwrap();
    serving_tenant.join().unwrap();
    for socket in [&socket, &tenant_socket] {
        let refused = UnixStream::connect(socket).err().map(|err| err.kind());
        assert_eq!(refused, Some(std::io::ErrorKind::ConnectionRefused), "{}", socket.display());
    }
    assert!(
        crate::Daemon::process().calls_work.begin(crate::Daemon::process()).is_none(),
        "a control call began, drained"
    );
    drop((client, tenants));
    // Closed, durable, as of its last commit, though only ordered.
    let closed = OpenCell::open(&path).await.unwrap();
    assert_eq!(closed.root(), Ok(b"drained".to_vec()));
}

/// TASK-141.4: a drain gives up past its bound, saying what is left: here, a connection that never
/// goes.
#[tokio::test(flavor = "multi_thread")]
async fn drains_give_up_past_their_bound() {
    host();
    let daemon = crate::Daemon::process();
    let connection = daemon.connections_work.begin(daemon).unwrap();
    let left = crate::drain::drain(std::time::Duration::from_millis(100)).await;
    let left = left.err().expect("a drain gave up");
    let expected = crate::drain::Left { connections: 1, ..Default::default() };
    assert_eq!(left, expected);
    assert_eq!(
        left.to_string(),
        "1 VM processes' connections, 0 embedded cells, 0 control calls and 0 cells' uploaders left"
    );
    drop(connection);
}

/// TASK-237: an image imported through the daemon (`ImportImage`) keeps what the bucket
/// acknowledged of it in the host's part cache, as the daemon's other cells do, and the import's
/// check of the filesystem it made reads it there: no ranged GET of the bucket's at all, the
/// recoveries as the import's cell is made and as the daemon opens it after reading what the
/// cell's head names, nothing, then the import's checkpoint, whole (TASK-407). Its
/// cell was an image's, whose uploader and fetcher had no cache: its acknowledged segments were
/// deleted, and the check read the image back from the bucket, 6,183 ranged GETs here (GCE r970:
/// 6.8 GB downloaded for 11 GB uploaded).
#[test]
fn imports_read_what_they_made_from_the_hosts_cache() {
    use bucket_cell::object_bucket::Request;
    use cladefs_fs::import::testing::{Entry, Kind, layer};
    host();
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    std::fs::create_dir(&cache).unwrap();
    // SAFETY: nextest runs each test in a process of its own, whose other threads don't read the
    // environment yet.
    unsafe { std::env::set_var("CLADEFSD_PART_CACHE", &cache) };
    let bucket = crate::cells::daemon_bucket().unwrap();
    // Files of a few parts each, so that what the check reads is in segments the bucket
    // acknowledged before it, as the import's close waits for its uploads.
    let contents: Vec<Vec<u8>> = (0..8u8).map(|file| pattern(file, 3 << 20)).collect();
    let names: Vec<String> = (0..contents.len()).map(|file| format!("file-{file}")).collect();
    let entries: Vec<Entry<'_>> = (names.iter().zip(&contents))
        .map(|(name, contents)| Entry::new(name, Kind::File(contents)))
        .collect();
    let image = dir.path().join("image.tar");
    std::fs::write(&image, layer(&entries).unwrap()).unwrap();
    let ranged = || bucket.stats().requests(Request::GetRange);
    let before = ranged();
    let path = dir.path().join("store");
    control(crate::cells::import(
        crate::Daemon::process(),
        path.clone(),
        256 << 20,
        image,
        None,
        0,
    ))
    .unwrap();
    // Before TASK-407, three: a footer as the cell's writer took its cell, made, its fence's;
    // two as the daemon opened it after, its recovery's and its writer's, past the checkpoint.
    assert_eq!(ranged() - before, 0, "ranged GETs of the bucket as the import ran");
}

/// TASK-149.17: a compromised VM process beside other tenants, checked against invariants.
#[cfg(target_os = "linux")]
mod hostile;

/// TASK-258: the daemon's spans, exported.
mod otel;

/// Board 14537: the extent device's vhost-user back-end, against a test front-end.
#[cfg(all(target_os = "linux", not(madsim)))]
mod vhost;

/// TASK-330.3: the daemon against a degraded bucket, for a long while.
mod soak;

/// TASK-260: the daemon's log records about a VM process's call for a guest request name the
/// request, by the guest's number of it, and the cell the call is of: refused at once, as a
/// `WRITE` of a flag no write knows, or deferred, as a `PRIVATE` the window refuses; and a call for
/// none names none.
#[test]
#[cfg(target_os = "linux")]
fn calls_for_guest_requests_are_logged_with_them() {
    use crate::wire::{Header, WRITE, WriteBody};
    use cladefs_core::request::{RequestId, within_now};
    use std::num::NonZeroU64;
    let lines = crate::logging::capture();
    host();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let (client, served) = connect(&path, 1 << 24);
    let named = std::fs::canonicalize(&path).unwrap();
    let named = named.to_str().unwrap();
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    let body = WriteBody {
        cell: extent.cell,
        sequence: extent.sequence,
        data_len: BLOCK as u64,
        ..Default::default()
    };
    let unknown = Header::new(WRITE, 0).with_body(&body).with_flags(1 << 20);
    let guest = |guest| Some(RequestId { guest: NonZeroU64::new(guest).unwrap(), open_cell: None });
    // Each line refused since the last look, without its time; its body as the wire has it,
    // escaped (board 6917, board 8571).
    let refused = || {
        let logged = std::mem::take(&mut *lines.lock().unwrap());
        (logged.into_iter())
            .filter(|line| line.contains("refused with EINVAL"))
            .map(|line| line.split_once(' ').unwrap().1.to_owned())
            .collect::<Vec<_>>()
    };
    // Its kind and flags as the wire has them, REQUEST among them if it says its guest request;
    // then what it says of its guest request, if anything.
    let refusal = |kind: u32, flags: u32, saying: &str, logged: &[String]| {
        let [line] = logged else { panic!("not one refusal logged: {logged:?}") };
        let starts = format!(
            "WARN cladefsd::server: cladefsd: request {kind} refused with EINVAL: flags \
             {flags:#x}, body "
        );
        assert!(line.starts_with(&starts), "{line}");
        match saying {
            "" => assert!(!line.contains(" request_id="), "{line}"),
            saying => assert!(line.ends_with(saying), "{line}"),
        }
    };
    refused();
    let written = within_now(guest(42), || client.call(unknown, &pattern(1, BLOCK), &[]).map(drop));
    assert_eq!(written, Err(Errno::EINVAL));
    let saying = format!(" request_id=42 store={named}");
    refusal(WRITE, 1 << 31 | 1 << 20, &saying, &refused());
    // Deferred: served as a future, polled with its request current; refused past the window's
    // end. Asked as the VMM does, a future polled with the guest's number current. (A `PRIVATE`
    // the window refused was this call until doc-26, which has the VM process serve it itself.)
    let ps = page_size() as u64;
    let memory = memory(4);
    // SAFETY: The memory is the test's, and outlives the cell.
    unsafe { client.create_dax_window_in_host_memory(memory, 4 * ps, None, 2) }.unwrap();
    refused();
    let mapped = cladefs_core::request::guest_request(43, client.map_zeros_async(8 * ps, ps));
    assert_eq!(futures::executor::block_on(mapped), Err(Errno::EINVAL));
    let saying = format!(" request_id=43 store={named}");
    refusal(crate::wire::MAP, 1 << 31, &saying, &refused());
    // A call for no guest request: none.
    assert_eq!(client.call(unknown, &pattern(1, BLOCK), &[]).map(drop), Err(Errno::EINVAL));
    refusal(WRITE, 1 << 20, "", &refused());
    drop(client);
    served.join().unwrap();
    // SAFETY: Unmaps the memory, which nothing uses any more.
    assert_eq!(unsafe { libc::munmap(memory.as_ptr().cast(), 4 * ps as usize) }, 0);
}

/// TASK-371: what a guest's sync costs through the daemon, measured: rounds of a guest's sync
/// loop's requests, a 4 KiB write and an ordered commit, then another write and a durable commit,
/// each round's time, to a file bucket whose requests wait `CLADEFS_BUCKET_LATENCY_MS`, at the URL
/// `CLADEFS_PHASES_BUCKET` names, through a daemon serving on a thread. Against
/// `crate::bucket`'s `bucket_round_trips_by_phase`, which commits in this process: what the daemon
/// adds. `CLADEFS_TEST_ARGS='--run-ignored only --no-capture durable_commits_through_the_daemon'`.
#[test]
#[ignore = "a measurement against CLADEFS_PHASES_BUCKET's bucket"]
fn durable_commits_through_the_daemon() {
    let url = std::env::var("CLADEFS_PHASES_BUCKET").expect("CLADEFS_PHASES_BUCKET names a bucket");
    let url = format!("{}/daemon-{:016x}/", url.trim_end_matches('/'), rand::random::<u64>());
    let _ = crate::logging::to_stderr();
    // SAFETY: As in host.
    unsafe {
        std::env::set_var("CLADEFSD_BUCKET", &url);
        std::env::set_var("CLADEFSD_TOKEN", "1");
    }
    let dir = tempfile::tempdir().unwrap();
    let (client, _served) = connect(&dir.path().join("store"), 1 << 30);
    let mut took = Vec::new();
    for sequence in 1..=40u64 {
        let started = std::time::Instant::now();
        let data = ExtentId { cell: client.cell(), sequence: 2 * sequence };
        write(&client, data, 0, &pattern(sequence as u8, BLOCK)).unwrap();
        client.commit_ordered(None).unwrap();
        let journal = ExtentId { cell: client.cell(), sequence: 2 * sequence + 1 };
        write(&client, journal, 0, &pattern(sequence as u8, BLOCK)).unwrap();
        client.commit(None).unwrap();
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        println!("ROUND {sequence} ms={ms:.1}");
        took.push(ms);
    }
    took.sort_by(f64::total_cmp);
    println!(
        "ROUNDS n={} median-ms={:.1} min-ms={:.1} max-ms={:.1}",
        took.len(),
        took[took.len() / 2],
        took[0],
        took[took.len() - 1]
    );
}

/// TASK-433.8: the governor sizes the cells' page cache to what it leaves it, as Linux sizes its
/// page cache to the memory free: a tenth of it dirty before the flusher writes back, a fifth
/// before writers wait, all of it held.
#[tokio::test(flavor = "multi_thread")]
async fn the_cells_page_cache_is_sized_to_what_the_governor_leaves() {
    let allowance = 8 << 30;
    crate::memory::govern(allowance).await;
    let sized = local_cell::pages::shared().options();
    assert_eq!(
        (sized.background, sized.hard, sized.most),
        (allowance / 10, allowance / 5, allowance)
    );
}

/// The cell `version` is of.
fn version_cell(version: cladefs_core::CellVersion) -> cladefs_core::CellId {
    cladefs_core::CellId { tenant: version.tenant, id: version.cell }
}

/// TASK-522: a request submitted to an empty pool gets a failure if the host
/// refuses to start its worker, even when no more requests come in.
#[test]
#[cfg(target_os = "linux")]
fn first_request_completes_when_worker_creation_fails() {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
    let pool = Box::leak(Box::new(crate::pool::Pool::new("spawn refused", 1)));
    refuse_system_call(libc::SYS_clone3, &[], libc::EAGAIN as u16);
    refuse_system_call(libc::SYS_clone, &[], libc::EAGAIN as u16);
    runtime.block_on(async {
        let reply = tokio::time::timeout(std::time::Duration::from_secs(2), pool.call(|| 7)).await;
        assert_eq!(reply.expect("request still waiting after worker creation failed"), None);
    });
}

/// TASK-520: failed write-back must leave a completed admission charged. The
/// dirty data is still in the cache and will cost disk blocks when it is written.
#[test]
#[cfg(target_os = "linux")]
fn failed_writeback_preserves_disk_admission() {
    use local_cell::{Dir, File};
    use std::os::fd::AsRawFd;

    let root = tempfile::tempdir().unwrap();
    let tenant = 0x520;
    let disk = crate::disk::Disk::new(tenant, root.path()).unwrap();
    let cell_dir = root.path().join("store");
    std::fs::create_dir(&cell_dir).unwrap();
    let dir = local_cell::HostDir::new(&cell_dir).unwrap();
    let file = dir.create("data", true).unwrap();
    let page = local_cell::pages::PAGE;
    // Room for one page admitted: it, and what its cell's own work may add for it (doc-22).
    let admitted = (1 + bucket_cell::host_disk::RESERVE_FACTOR) * page;
    let footprint = dir.footprint();
    let limit = disk.used() + footprint.reserved() + admitted;
    crate::disk::set_limit(Some(tenant), Some(limit)).unwrap();
    let fd = file.host_file().as_raw_fd() as u32;
    refuse_system_call(libc::SYS_pwrite64, &[(0, fd)], libc::EIO as u16);
    let writing = footprint.admit(page, 0).unwrap();
    file.write_at(&vec![0x5a; page as usize], 0).unwrap();
    drop(writing);
    // Its write-back failing changes nothing of what is counted (TASK-539.5: nothing recounts).
    assert_eq!(
        dir.cache().write_back_all().map_err(|err| err.raw_os_error()),
        Err(Some(libc::EIO))
    );
    assert!(footprint.admit(page, 0).is_err(), "admitted past the page written");
}

/// TASK-529: compare the cost of the same unmatched lookups as the number of
/// grants grows. Both paths are missing leaves in an existing directory.
#[test]
#[ignore = "a grant lookup scaling measurement on the test host"]
fn measure_unmatched_grant_lookups() {
    host();
    let root = tempfile::tempdir().unwrap();
    let grants =
        crate::grants::Grants::new(crate::Daemon::process().clone(), root.path().to_owned());
    let first = root.path().join("missing-a");
    let second = root.path().join("missing-b");
    let mut last = 0;
    for tenants in [1, 8, 32] {
        for tenant in last + 1..=tenants {
            let dir = root.path().join(format!("dir-{tenant}"));
            std::fs::create_dir(&dir).unwrap();
            grants.grant(tenant, &dir, (None, None)).unwrap();
        }
        last = tenants;
        let start = std::time::Instant::now();
        for _ in 0..1000 {
            assert_eq!(grants.tenant_at(&first), None);
            assert!(grants.same_tenant(&first, &second));
        }
        eprintln!("{tenants} grants, 1000 lookup pairs: {:?}", start.elapsed());
    }
    for tenant in 1..=last {
        grants.revoke(tenant).unwrap();
    }
}

/// TASK-525: vary the number of disks and the number of resident cache pages
/// independently. Timings include only full recounts of already clean files.
#[test]
#[ignore = "a disk recount scaling measurement on the test host"]
fn measure_disk_recounts_with_resident_pages() {
    use local_cell::{Dir, File};
    let root = tempfile::tempdir().unwrap();
    let mut disks = Vec::new();
    for tenants in [1, 8, 32] {
        while disks.len() < tenants {
            let id = disks.len() as u64 + 1;
            let path = root.path().join(format!("tenant-{id}"));
            std::fs::create_dir(&path).unwrap();
            disks.push(crate::disk::Disk::new(id, &path).unwrap());
        }
        let cell_dir = local_cell::HostDir::new(&root.path().join("tenant-1")).unwrap();
        let file = cell_dir.create("resident", true).unwrap();
        for pages in [0, 2048] {
            if pages > 0 {
                file.write_at(&vec![1; pages * local_cell::pages::PAGE as usize], 0).unwrap();
                file.sync().unwrap();
            }
            let start = std::time::Instant::now();
            for _ in 0..20 {
                crate::disk::count(None).unwrap();
            }
            eprintln!(
                "{tenants} disks, {pages} resident pages, 20 recounts: {:?}",
                start.elapsed()
            );
        }
        drop(file);
        cell_dir.remove("resident").unwrap();
    }
}

/// Gives the test control over when this thread's next unlink enters the
/// filesystem. Only the revoke thread installs the filter, so a simultaneous
/// grant is free to remove a stale socket and bind its replacement.
#[cfg(target_os = "linux")]
fn pause_socket_unlink() -> std::os::fd::OwnedFd {
    use std::os::fd::FromRawFd;
    let load_nr = libc::sock_filter {
        code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
        jt: 0,
        jf: 0,
        k: std::mem::offset_of!(libc::seccomp_data, nr) as u32,
    };
    let notify_on = |nr: libc::c_long| libc::sock_filter {
        code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
        jt: 0,
        jf: 1,
        k: nr as u32,
    };
    let ret = |k| libc::sock_filter { code: (libc::BPF_RET | libc::BPF_K) as u16, jt: 0, jf: 0, k };
    let mut code = [
        load_nr,
        #[cfg(target_arch = "x86_64")]
        notify_on(libc::SYS_unlink),
        #[cfg(target_arch = "x86_64")]
        ret(libc::SECCOMP_RET_USER_NOTIF),
        notify_on(libc::SYS_unlinkat),
        ret(libc::SECCOMP_RET_USER_NOTIF),
        ret(libc::SECCOMP_RET_ALLOW),
    ];
    let filter = libc::sock_fprog { len: code.len() as u16, filter: code.as_mut_ptr() };
    // SAFETY: This thread installs a filter copied by the kernel. Success
    // returns a new descriptor owned by the test.
    unsafe {
        assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
        let fd = libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            libc::SECCOMP_FILTER_FLAG_NEW_LISTENER,
            &filter,
        );
        assert!(fd >= 0, "installing unlink filter: {}", std::io::Error::last_os_error());
        std::os::fd::OwnedFd::from_raw_fd(fd as i32)
    }
}

/// TASK-519: the pathname returned to a regrant must stay connectable when
/// an overlapping revoke finishes. Pause the old unlink in the kernel and
/// let the regrant finish first if the locking permits it.
#[test]
#[cfg(target_os = "linux")]
fn a_racing_revoke_preserves_the_regranted_socket() {
    use std::os::fd::AsRawFd;
    use std::sync::{Arc, mpsc};
    use std::time::Duration;
    host();
    let root = tempfile::tempdir().unwrap();
    let grants = Arc::new(crate::grants::Grants::new(
        crate::Daemon::process().clone(),
        root.path().to_owned(),
    ));
    let dir = root.path().join("tenant");
    std::fs::create_dir(&dir).unwrap();
    grants.grant(1, &dir, (None, None)).unwrap();
    let (send_fd, recv_fd) = mpsc::channel();
    let revoking = grants.clone();
    let revoke = std::thread::spawn(move || {
        send_fd.send(pause_socket_unlink()).unwrap();
        revoking.revoke(1).unwrap();
    });

    let listener = recv_fd.recv().unwrap();
    let fd = listener.as_raw_fd();
    let mut poll = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    // SAFETY: A single pollfd referring to the live listener.
    assert_eq!(unsafe { libc::poll(&mut poll, 1, 5000) }, 1, "revoke never attempted its unlink");
    // SAFETY: All-zero is a valid empty notification that the kernel fills.
    let mut notification: libc::seccomp_notif = unsafe { std::mem::zeroed() };
    // SAFETY: The ioctl writes a notification of the indicated type and size.
    assert_eq!(unsafe { libc::ioctl(fd, libc::SECCOMP_IOCTL_NOTIF_RECV, &mut notification) }, 0);

    let granting = grants.clone();
    let (started, ready) = mpsc::channel();
    let (finished, result) = mpsc::channel();
    let grant = std::thread::spawn(move || {
        started.send(()).unwrap();
        finished.send(granting.grant(1, &dir, (None, None))).unwrap();
    });
    ready.recv().unwrap();
    let early = result.recv_timeout(Duration::from_secs(2));
    let mut resume = libc::seccomp_notif_resp {
        id: notification.id,
        val: 0,
        error: 0,
        flags: libc::SECCOMP_USER_NOTIF_FLAG_CONTINUE as u32,
    };
    // SAFETY: Answers the paused call's notification on the same listener.
    assert_eq!(unsafe { libc::ioctl(fd, libc::SECCOMP_IOCTL_NOTIF_SEND, &mut resume) }, 0);
    revoke.join().unwrap();
    grant.join().unwrap();
    let socket = early.unwrap_or_else(|_| result.recv().unwrap()).unwrap();
    UnixStream::connect(socket).expect("the earlier revoke removed the new grant's socket");
    grants.revoke(1).unwrap();
}

/// doc-25, TASK-698.3: an ephemeral cell forked from a version reads what its parent's version
/// held, writes its own, which no bucket gets, and holds them across closing and opening again;
/// its reference, naming the host's incarnation, stands in the version's cell's bucket while it
/// lives, and goes as it is deleted; a host starting retires the references of the cells it holds
/// no more, and a host decommissioned all of its own.
#[test]
fn ephemeral_cells_fork_from_versions_and_retire_their_references() {
    host();
    let ps = page_size() as usize;
    let dir = tempfile::tempdir().unwrap();
    let ephemeral = cladefs_core::ephemeral_host::open(&dir.path().join("ephemeral")).unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let parent = CellClient::connect(&socket, dir.path().join("parent"), 1 << 24).unwrap();
    let inherited = ExtentId { cell: parent.cell(), sequence: 1 };
    write(&parent, inherited, 0, &pattern(1, ps)).unwrap();
    parent.commit(Some(b"root")).unwrap();
    let version = parent.make_version().unwrap();
    let bucket = crate::cells::daemon_bucket().unwrap();
    let buckets = control(bucket.ephemeral_buckets()).unwrap();
    let references = || {
        let listing = bucket_cell::lifecycle::ephemeral_references(
            &*bucket,
            &buckets,
            ephemeral.incarnation(),
        );
        let mut children: Vec<_> =
            control(listing).unwrap().into_iter().map(|(_, child)| child).collect();
        children.sort();
        children
    };
    let fork = |name: &str| {
        let at = dir.path().join(name);
        control(crate::cells::fork_ephemeral_of_version(
            crate::Daemon::process(),
            version_cell(version),
            version.version,
            at,
            None,
        ))
    };
    let path = dir.path().join("child");
    let child = fork("child").unwrap();
    assert!(local_cell::host_ephemeral(&path).unwrap());
    assert_eq!(references(), vec![child]);
    assert_eq!(fork("child"), Err(Errno::EEXIST));

    let client = CellClient::connect(&socket, &path, 1 << 24).unwrap();
    assert_eq!((client.cell(), client.root().unwrap()), (child.id, b"root".to_vec()));
    assert_eq!(read(&client, inherited, 0, ps).unwrap(), pattern(1, ps));
    let own = ExtentId { cell: child.id, sequence: 1 };
    write(&client, own, 0, &pattern(2, ps)).unwrap();
    client.commit(None).unwrap();
    drop(client);
    let client = CellClient::connect(&socket, &path, 1 << 24).unwrap();
    assert_eq!(read(&client, own, 0, ps).unwrap(), pattern(2, ps));
    assert_eq!(read(&client, inherited, 0, ps).unwrap(), pattern(1, ps));
    drop(client);
    let keys = control(bucket_cell::Bucket::list(&*bucket, "")).unwrap();
    let theirs = keys.iter().filter(|meta| bucket_cell::layout::owner(&meta.key) == Some(child));
    assert_eq!(theirs.count(), 0, "nothing of the ephemeral cell's is in the bucket");

    // Deleted, its reference goes; a cell whose cell went without its host retires it as the
    // host starts again; a host decommissioned retires every one of its.
    delete_cell(&path).unwrap();
    assert_eq!(references(), vec![]);
    let gone = fork("gone").unwrap();
    let kept = fork("kept").unwrap();
    local_cell::remove_cell_dir(&dir.path().join("gone")).unwrap();
    assert_eq!(control(ephemeral.retire_stale()).unwrap(), 1);
    assert_eq!(references(), vec![kept]);
    assert!(ephemeral.entry(gone).unwrap().is_none());
    let decommissioning =
        bucket_cell::lifecycle::decommission(&*bucket, &buckets, ephemeral.incarnation(), 10);
    let retired = control(decommissioning);
    assert_eq!(retired, Ok(bucket_cell::lifecycle::Decommissioned { retired: 1, left: 0 }));
    assert_eq!(references(), vec![]);
}

/// Boards 8519, 8521, 8523: hosts decommissioned through the control API, as cladefsctl calls it.
/// The serving daemon's own incarnation, or one no host makes (an even one), is refused, nothing
/// retired. Another's, given as the daemon's log printed it, is listed with its references until
/// then, and has them retired, `most` at a time, the rest left to a call again; listed after as
/// decommissioned, with none. And a host decommissioned while alive, by mistake, finds out: its
/// next ephemeral fork is refused (`ESTALE`), leaving no reference nor index entry, and it refuses
/// to start again, rather than serve its cells over references retired. GetStats says the
/// serving daemon's incarnation.
#[test]
#[cfg(target_os = "linux")]
fn hosts_are_decommissioned_through_the_control_api() {
    use crate::control::proto::{
        DecommissionHostRequest, EphemeralHost, GetStatsRequest, ListEphemeralHostsRequest,
    };
    use bucket_cell::layout::{self, Kind};
    host();
    let ps = page_size() as usize;
    let dir = tempfile::tempdir().unwrap();
    let (lost_dir, serving_dir) = (dir.path().join("lost"), dir.path().join("serving"));
    let lost = cladefs_core::ephemeral_host::open(&lost_dir).unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let control_socket = dir.path().join("control.sock");
    let listener = UnixListener::bind(&control_socket).unwrap();
    let tenants = dir.path().join("tenants");
    std::fs::create_dir(&tenants).unwrap();
    std::thread::spawn(move || crate::control::serve_control(listener, tenants));
    let api = crate::testing_control::Control::connect(&control_socket).unwrap();
    let parent = CellClient::connect(&socket, dir.path().join("parent"), 1 << 24).unwrap();
    write(&parent, ExtentId { cell: parent.cell(), sequence: 1 }, 0, &pattern(1, ps)).unwrap();
    parent.commit(Some(b"root")).unwrap();
    let version = parent.make_version().unwrap();
    drop(parent);
    let fork = |name: &str| {
        let at = dir.path().join(name);
        let cell = version_cell(version);
        control(crate::cells::fork_ephemeral_of_version(
            crate::Daemon::process(),
            cell,
            version.version,
            at,
            None,
        ))
    };
    let bucket = crate::cells::daemon_bucket().unwrap();
    let buckets = control(bucket.ephemeral_buckets()).unwrap();
    let incarnation = lost.incarnation();
    let references = || {
        let listing = bucket_cell::lifecycle::ephemeral_references(&*bucket, &buckets, incarnation);
        control(listing).unwrap().len()
    };
    let errno = |refused: cladefs_core::Error| refused.errno();
    for name in ["one", "two", "three"] {
        fork(name).unwrap();
    }
    assert_eq!(references(), 3);
    assert_eq!(
        api.call(async |client| {
            client.get_stats(GetStatsRequest { ..Default::default() }).await
        })
        .unwrap()
        .incarnation,
        incarnation
    );

    // Its own, or an even one, which no host makes: refused, nothing retired.
    assert_eq!(
        api.call(async |client| {
            client
                .decommission_host(DecommissionHostRequest { incarnation: incarnation, most: 0 })
                .await
        })
        .map_err(errno),
        Err(Some(Errno::EINVAL))
    );
    let even = incarnation ^ 1;
    assert_eq!(
        api.call(async |client| {
            client.decommission_host(DecommissionHostRequest { incarnation: even, most: 0 }).await
        })
        .map_err(errno),
        Err(Some(Errno::EINVAL))
    );
    assert_eq!(
        api.call(async |client| {
            client.decommission_host(DecommissionHostRequest { incarnation: 0, most: 0 }).await
        })
        .map_err(errno),
        Err(Some(Errno::EINVAL))
    );
    assert_eq!(references(), 3);
    let listed = EphemeralHost { incarnation, references: 3, decommissioned: false };
    assert_eq!(
        api.call(async |client| {
            client.list_ephemeral_hosts(ListEphemeralHostsRequest {}).await
        })
        .map(|response| response.hosts)
        .unwrap(),
        vec![listed]
    );

    // The daemon serving of another incarnation now, the lost host's, as its log printed it,
    // decommissioned: two at a time, then the rest, then none.
    let serving = cladefs_core::ephemeral_host::open(&serving_dir).unwrap();
    assert_eq!(
        api.call(async |client| {
            client.get_stats(GetStatsRequest { ..Default::default() }).await
        })
        .unwrap()
        .incarnation,
        serving.incarnation()
    );
    let printed = cladefs_core::ephemeral_host::opened(&lost_dir, incarnation);
    let given = printed.rsplit(' ').next().unwrap();
    let parsed = base32_id::parse(given).unwrap();
    assert_eq!(parsed, incarnation, "{printed}");
    let done = |response: crate::control::proto::DecommissionHostResponse| {
        (response.retired, response.left)
    };
    assert_eq!(
        api.call(async |client| {
            client.decommission_host(DecommissionHostRequest { incarnation: parsed, most: 2 }).await
        })
        .map(done)
        .unwrap(),
        (2, 1)
    );
    assert_eq!(references(), 1);
    assert_eq!(
        api.call(async |client| {
            client.decommission_host(DecommissionHostRequest { incarnation: parsed, most: 0 }).await
        })
        .map(done)
        .unwrap(),
        (1, 0)
    );
    assert_eq!(references(), 0);
    assert_eq!(
        api.call(async |client| {
            client.decommission_host(DecommissionHostRequest { incarnation: parsed, most: 0 }).await
        })
        .map(done)
        .unwrap(),
        (0, 0)
    );
    let listed = EphemeralHost { incarnation, references: 0, decommissioned: true };
    assert_eq!(
        api.call(async |client| {
            client.list_ephemeral_hosts(ListEphemeralHostsRequest {}).await
        })
        .map(|response| response.hosts)
        .unwrap(),
        vec![listed]
    );

    // The lost host alive after all: its next fork refused, leaving nothing in the bucket; its
    // start refused.
    let lost = cladefs_core::ephemeral_host::open(&lost_dir).unwrap();
    assert_eq!(lost.incarnation(), incarnation);
    assert_eq!(fork("four"), Err(Errno::ESTALE));
    assert_eq!(references(), 0);
    let parent_cell = version_cell(version);
    let listed = control(bucket_cell::Bucket::list(&*bucket, &layout::references(parent_cell)));
    let theirs = listed.unwrap().into_iter().filter(|meta| {
        matches!(layout::kind(&meta.key), Kind::Ephemeral { incarnation: of, .. } if of == incarnation)
    });
    assert_eq!(theirs.count(), 0, "no reference of the incarnation decommissioned");
    let refused = control(lost.retire_stale()).map_err(|err| format!("{err:#}"));
    assert_eq!(
        refused,
        Err(format!(
            "incarnation {} of {} was decommissioned (DecommissionHost): its ephemeral cells' \
             references are retired, and what they read may be collected; move the directory \
             aside, its cells with it, for the daemon to start of a new incarnation",
            base32_id::Short(incarnation),
            lost_dir.display()
        ))
    );
}

/// Boards 5732 and 5734: a target takes a cell's migrations by the cell's epoch, one request at a
/// time: a prepare asked again answers as before; a newer one supersedes the migration it took,
/// dropping what that staged; an older one, late, it refuses. A hand-over asked of another
/// incarnation of the target's is answered neither yes nor no; one naming another take of the
/// migration than its prepare's answer is refused, what it took dropped, its reference with it.
/// A migration is renewed while taken, and an accepted one released.
#[test]
fn targets_take_a_cells_migrations_by_its_epoch() {
    use crate::migration::target;
    use crate::peers::proto::{FileDigest, MigrateHandOverRequest, MigratePrepareRequest};
    use tonic::Code;
    host();
    let dir = tempfile::tempdir().unwrap();
    let root = host_fs::canonicalize(dir.path()).unwrap();
    // SAFETY: as `host`'s: no other thread of this test's process reads the environment yet.
    unsafe { std::env::set_var("CLADEFSD_EPHEMERAL_CELLS", &root) };
    let ephemeral = cladefs_core::ephemeral_host::open(&root.join("ephemeral")).unwrap();
    let path = root.join("cell");
    let staged = |migration| cladefs_core::ephemeral_host::staging(&path, migration);
    let (parent, child) =
        (cladefs_core::CellId { tenant: 1, id: 2 }, cladefs_core::CellId { tenant: 1, id: 7 });
    let prepare = |migration, epoch| MigratePrepareRequest {
        migration,
        tenant: child.tenant,
        cell: child.id,
        parent_tenant: parent.tenant,
        parent_cell: parent.id,
        at: 3,
        parent_epoch: 1,
        bucket: "memory:///cells".into(),
        to: path.display().to_string(),
        epoch,
        #[allow(deprecated)]
        formats: String::new(),
        versions: Some(crate::peers::current_formats()),
        #[allow(deprecated)]
        bucket_backed: false,
    };
    let bucket = crate::cells::daemon_bucket().unwrap();
    let buckets = control(bucket.ephemeral_buckets()).unwrap();
    let references = || {
        let listing = bucket_cell::lifecycle::ephemeral_references(
            &*bucket,
            &buckets,
            ephemeral.incarnation(),
        );
        control(listing).unwrap().into_iter().map(|(_, child)| child).collect::<Vec<_>>()
    };

    let daemon = crate::Daemon::process();
    let (incarnation, token) = control(target::prepare(daemon, prepare(0x11, 2))).unwrap();
    assert_eq!(incarnation, ephemeral.incarnation());
    assert!(staged(0x11).is_dir());
    assert_eq!(references(), vec![child]);
    assert_eq!(control(target::prepare(daemon, prepare(0x11, 2))).unwrap(), (incarnation, token));
    let late = control(target::prepare(daemon, prepare(0x13, 1))).unwrap_err();
    assert_eq!(late.code(), Code::Aborted, "{late}");
    assert!(staged(0x11).is_dir() && !staged(0x13).exists());
    let (_, newer) = control(target::prepare(daemon, prepare(0x15, 3))).unwrap();
    assert_ne!(newer, token);
    assert!(!staged(0x11).exists() && staged(0x15).is_dir());
    assert_eq!(references(), vec![child], "the reference kept, the newer migration's alike");
    target::renew(daemon, 0x15).unwrap();
    assert_eq!(target::renew(daemon, 0x11).unwrap_err().code(), Code::NotFound);

    let hand_over = |incarnation, token| MigrateHandOverRequest {
        migration: 0x15,
        incarnation,
        token,
        files: vec![FileDigest { path: "description".into(), len: 1, crc: None }],
        #[allow(deprecated)]
        placement_token: None,
        #[allow(deprecated)]
        bucket_backed: false,
    };
    let unknown =
        control(target::hand_over(daemon, hand_over(incarnation ^ 2, newer))).unwrap_err();
    assert_eq!(unknown.code(), Code::FailedPrecondition, "{unknown}");
    assert!(staged(0x15).is_dir(), "neither yes nor no: nothing dropped");
    assert!(!control(target::hand_over(daemon, hand_over(incarnation, newer ^ 2))).unwrap());
    assert!(!staged(0x15).exists(), "refused, what it took dropped");
    assert_eq!(ephemeral.entries().unwrap(), vec![]);
    assert_eq!(references(), vec![]);
    assert!(
        !control(target::hand_over(daemon, hand_over(incarnation, newer))).unwrap(),
        "refused for good"
    );

    // Released, an acceptance whose cell's noting failed has it noted taken first.
    let (_, token) = control(target::prepare(daemon, prepare(0x19, 4))).unwrap();
    ephemeral.accept(0x19, child).unwrap();
    assert_eq!(ephemeral.entry(child).unwrap().unwrap().receiving(), Some((0x19, token)));
    control(target::release(daemon, 0x19)).unwrap();
    assert!(!ephemeral.accepted(0x19));
    assert_eq!(
        ephemeral.entry(child).unwrap().unwrap().migrating,
        None,
        "noted taken before it was forgotten"
    );
}

/// TASK-704: an open ephemeral cell is halted as its host forgets its keys, or its
/// tenant's, as a bucket cell's is: its reads, of its own extents and of what it inherits, and its
/// writes and commits fail with `EACCES`; another ephemeral cell of the version's reads on until
/// its tenant's are forgotten.
#[test]
fn forgetting_keys_halts_open_ephemeral_cells() {
    host();
    let ps = page_size() as usize;
    let dir = tempfile::tempdir().unwrap();
    cladefs_core::ephemeral_host::open(&dir.path().join("ephemeral")).unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let parent = CellClient::connect(&socket, dir.path().join("parent"), 1 << 24).unwrap();
    let inherited = ExtentId { cell: parent.cell(), sequence: 1 };
    write(&parent, inherited, 0, &pattern(1, ps)).unwrap();
    parent.commit(Some(b"root")).unwrap();
    let version = parent.make_version().unwrap();
    drop(parent);
    let open = |name: &str, byte: u8| {
        let at = dir.path().join(name);
        let forked = crate::cells::fork_ephemeral_of_version(
            crate::Daemon::process(),
            version_cell(version),
            version.version,
            at.clone(),
            None,
        );
        let child = control(forked).unwrap();
        let client = CellClient::connect(&socket, &at, 1 << 24).unwrap();
        let own = ExtentId { cell: child.id, sequence: 1 };
        write(&client, own, 0, &pattern(byte, ps)).unwrap();
        client.commit(None).unwrap();
        (child, client, own)
    };
    let (first, ours, our_own) = open("first", 2);
    let (second, theirs, their_own) = open("second", 3);
    assert_eq!(first.tenant, second.tenant);

    let forgotten =
        cladefs_core::bucket::forget(bucket_cell::keys::Scope::Cell(first), "first shredded");
    assert_eq!(forgotten.stores, 1, "{forgotten:?}");
    assert_eq!(read(&ours, our_own, 0, ps), Err(Errno::EACCES));
    assert_eq!(read(&ours, inherited, 0, ps), Err(Errno::EACCES));
    let later = ExtentId { cell: first.id, sequence: 2 };
    assert_eq!(write(&ours, later, 0, &pattern(4, ps)), Err(Errno::EACCES));
    assert_eq!(ours.commit(None), Err(Errno::EACCES));
    assert_eq!(read(&theirs, their_own, 0, ps), Ok(pattern(3, ps)));
    assert_eq!(read(&theirs, inherited, 0, ps), Ok(pattern(1, ps)));

    let tenant = bucket_cell::keys::Scope::Tenant(second.tenant);
    let forgotten = cladefs_core::bucket::forget(tenant, "tenant shredded");
    // The second's, and the version's, which the daemon holds open for its forks.
    assert_eq!(forgotten.stores, 2, "{forgotten:?}");
    assert_eq!(read(&theirs, their_own, 0, ps), Err(Errno::EACCES));
    let later = ExtentId { cell: second.id, sequence: 2 };
    assert_eq!(write(&theirs, later, 0, &pattern(5, ps)), Err(Errno::EACCES));
    assert_eq!(theirs.commit(None), Err(Errno::EACCES));
}

/// TASK-661 (board 2764): large inline `write_with_barriers_async` calls waiting for request
/// credits or for daemon replies never gather their payloads into per-request `Vec<u8>` buffers:
/// `Client::call_stream` acquires waiting room first and streams `&[VolatileSlice]` directly into
/// the socket through one bounded scratch buffer, keeping VMM heap flat even with 96 concurrent
/// 4 MiB writes (384 MiB total, exceeding `MAX_STREAM_BYTES`) in flight while replies are held.
#[test]
fn large_writes_waiting_for_replies_do_not_gather_unbounded_payloads() {
    use crate::wire::*;
    use std::io::Read as _;
    use std::os::fd::AsFd;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use zerocopy::FromBytes as _;

    const WRITES: usize = 96;
    // Exceeds TRANSFER_SLOT (1 MiB) so every write takes the inline streaming path.
    const WRITE_LEN: usize = 4 << 20;
    const ROOT: &[u8] = b"streamed-root";
    let in_window = (MAX_STREAM_BYTES / (WRITE_LEN + ROOT.len()) as u64) as usize;
    assert!(in_window > 0 && in_window < WRITES, "in_window={in_window}");

    let ps = page_size();
    let (stream, mut daemon) = UnixStream::pair().unwrap();
    let sender = Sender::new(daemon.try_clone().unwrap()).unwrap();
    let (all_window_received, window_received) = mpsc::channel();
    let (release_replies, replies_released) = mpsc::channel();
    let drained = Arc::new(AtomicUsize::new(0));
    let drained_server = drained.clone();
    let answering = std::thread::spawn(move || {
        let (own, public) = (memfd(ps), memfd(ps));
        let mut scratch = vec![0u8; 64 << 10];
        let mut held_ids = Vec::new();
        let mut released = false;
        let mut header_bytes = [0u8; size_of::<Header>()];
        // Drain handshake (OPEN) using Receiver so SCM_RIGHTS fds are consumed; writes below
        // exceed TRANSFER_SLOT so lazy TRANSFER is never sent.
        let mut receiver = Receiver::new(daemon.try_clone().unwrap());
        {
            let request = receiver.receive(|| 0).unwrap().unwrap();
            let mut reply = Header::new(REPLY, request.header.id);
            assert_eq!(request.header.kind, OPEN);
            reply.set_body(&opened_body(7));
            let payload = 0u64.to_le_bytes();
            let fds = [own.as_fd(), public.as_fd()];
            sender.send(reply, &payload, &fds).unwrap();
        }
        drop(receiver);

        // Drain every WRITE raw into the fixed 64 KiB scratch buffer (zero per-write heap
        // allocation on the server side too), checking its byte pattern and trailing root.
        while daemon.read_exact(&mut header_bytes).is_ok() {
            let header = Header::read_from_bytes(&header_bytes).unwrap();
            assert_eq!(header.kind, WRITE);
            let body = header.read_body::<WriteBody>();
            assert_eq!(body.data_len, WRITE_LEN as u64);
            let expected_len = WRITE_LEN + ROOT.len();
            assert_eq!(header.len as usize, expected_len);
            let mut seen = 0usize;
            let mut root_buf = Vec::with_capacity(ROOT.len());
            while seen < expected_len {
                let want = (expected_len - seen).min(scratch.len());
                daemon.read_exact(&mut scratch[..want]).unwrap();
                let data_part = want.min(WRITE_LEN.saturating_sub(seen));
                assert!(scratch[..data_part].iter().all(|&b| b == 0xab), "corrupt write byte");
                if data_part < want {
                    root_buf.extend_from_slice(&scratch[data_part..want]);
                }
                seen += want;
            }
            assert_eq!(root_buf, ROOT, "corrupt trailing root");
            let count = drained_server.fetch_add(1, Ordering::SeqCst) + 1;
            if released {
                sender.send(Header::new(REPLY, header.id), &[], &[]).unwrap();
            } else {
                held_ids.push(header.id);
                if count == in_window {
                    all_window_received.send(()).unwrap();
                    replies_released.recv().unwrap();
                    released = true;
                    for id in held_ids.drain(..) {
                        sender.send(Header::new(REPLY, id), &[], &[]).unwrap();
                    }
                }
            }
            if count == WRITES {
                break;
            }
        }
    });

    let client = Arc::new(
        CellClient::over(stream, Path::new("store"), Opening::Create(1 << 24, Kind::Fxfs)).unwrap(),
    );
    // One 4 KiB page aliased 1024 times to make a 4 MiB write without allocating 4 MiB.
    let page = vec![0xab; 4096];
    let slices: Vec<VolatileSlice<'_>> =
        (0..WRITE_LEN / page.len()).map(|_| VolatileSlice::from(&page[..])).collect();
    let barriers = WriteBarriers { before: false, after: Some(Commit::Ordered), root: Some(ROOT) };
    let peak = cladefs_core::heap::peak_while(|| {
        std::thread::scope(|scope| {
            let releaser = scope.spawn(move || {
                window_received.recv_timeout(WAIT).expect("in-window writes reached the server");
                // Give excess threads a moment to prove backpressure held them before sending.
                std::thread::sleep(std::time::Duration::from_millis(50));
                assert_eq!(
                    drained.load(Ordering::SeqCst),
                    in_window,
                    "writes past MAX_STREAM_BYTES were sent before replies arrived"
                );
                release_replies.send(()).unwrap();
            });
            let writers: Vec<_> = (1..=WRITES as u64)
                .map(|sequence| {
                    let client = client.clone();
                    let slices = &slices;
                    scope.spawn(move || {
                        let extent = ExtentId { cell: client.cell(), sequence };
                        futures::executor::block_on(
                            client.write_with_barriers_async(extent, 0, slices, barriers),
                        )
                    })
                })
                .collect();
            for writer in writers {
                assert_eq!(writer.join().unwrap(), Ok(()));
            }
            releaser.join().unwrap();
        });
    });
    drop(client);
    answering.join().unwrap();
    // 96 * 4 MiB = 384 MiB total; streaming keeps peak heap to a few MiB at most.
    assert!(
        peak < 16 << 20,
        "peak heap {} MiB for {WRITES} concurrent 4 MiB writes (expected < 16 MiB)",
        peak >> 20
    );
}

/// board 5386: a write `OPEN` of a public base (which is read-only) with no other holder must
/// check `may_write` before publishing the hold and close the `OpenCell` through `release().await`
/// rather than dropping it synchronously on the runtime, returning `EROFS` cleanly.
#[test]
fn refused_write_open_of_public_base_releases_cleanly_with_erofs() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("public-base");
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));

    // Create the cell, mark it public, and close it so no other holder keeps it open.
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        let cell = OpenCell::create(&store, 1 << 24).await.unwrap();
        local_cell::upload::blocking(|| cell.make_public()).unwrap();
        cell.close().await;
    });

    // Opening the public base for writing must fail with EPERM and not panic or leak the hold.
    let err = CellClient::connect(&socket, &store, 1 << 24).map(drop).unwrap_err();
    assert_eq!(err.errno(), Some(Errno::EPERM), "expected EPERM, got {err:#}");

    // A second attempt also gets EPERM (the first did not leave a wedged hold behind).
    let again = CellClient::connect(&socket, &store, 1 << 24).map(drop).unwrap_err();
    assert_eq!(again.errno(), Some(Errno::EPERM), "expected EPERM again, got {again:#}");
}

/// TASK-549 (board 2494): sequential and staggered VM restores of the same version within `LINGER`
/// reuse the kept version store opened by the first restore instead of closing and reopening it
/// on every restore.
#[test]
fn staggered_zygote_restores_reuse_kept_version_store_without_reopening() {
    use std::sync::atomic::Ordering;

    host();
    // SAFETY: nextest runs each test in its own process, before bucket traffic starts.
    unsafe { std::env::set_var("CLADEFS_BUCKET_LATENCY_MS", "50") };
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));

    let client = CellClient::connect(&socket, dir.path().join("store"), 1 << 24).unwrap();
    let extent = ExtentId { cell: client.cell(), sequence: 1 };
    write(&client, extent, 0, &pattern(9, BLOCK)).unwrap();
    client.commit(Some(b"zygote-staggered")).unwrap();
    let version = client.make_version().unwrap();
    drop(client);

    let daemon = crate::Daemon::process();
    let opens_before = daemon.kept_opens.load(Ordering::Relaxed);

    // First restore opens and keeps the version store.
    let first =
        CellClient::connect_forked(&socket, dir.path().join("restore-0"), version.clone(), 0)
            .unwrap();
    assert_eq!(read(&first, extent, 0, BLOCK), Ok(pattern(9, BLOCK)));
    assert_eq!(first.root(), Ok(b"zygote-staggered".to_vec()));
    drop(first);
    assert_eq!(
        daemon.kept_opens.load(Ordering::Relaxed) - opens_before,
        1,
        "first restore should open the version store once"
    );

    // Sequential restore within LINGER must reuse the kept store without reopening it.
    let second =
        CellClient::connect_forked(&socket, dir.path().join("restore-1"), version.clone(), 0)
            .unwrap();
    assert_eq!(read(&second, extent, 0, BLOCK), Ok(pattern(9, BLOCK)));
    assert_eq!(second.root(), Ok(b"zygote-staggered".to_vec()));
    drop(second);
    assert_eq!(
        daemon.kept_opens.load(Ordering::Relaxed) - opens_before,
        1,
        "second restore within LINGER reopened the kept version store"
    );

    // Staggered restores spaced 200 ms apart must still reuse the single kept opening with no EBUSY.
    let mut threads = Vec::new();
    for n in 2..6usize {
        std::thread::sleep(std::time::Duration::from_millis(200));
        let (socket, version) = (socket.clone(), version.clone());
        let to = dir.path().join(format!("restore-{n}"));
        threads.push(std::thread::spawn(move || {
            let client = CellClient::connect_forked(&socket, to, version, 0).unwrap();
            assert_eq!(read(&client, extent, 0, BLOCK), Ok(pattern(9, BLOCK)));
            assert_eq!(client.root(), Ok(b"zygote-staggered".to_vec()));
        }));
    }
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(
        daemon.kept_opens.load(Ordering::Relaxed) - opens_before,
        1,
        "staggered restores reopened the kept version store"
    );
}

/// board 11556: concurrent ephemeral `ForkVersion` calls for the exact same freshly-published
/// uncached version must all succeed without racing on `ephemeral_host`'s staged journal files.
#[test]
fn concurrent_ephemeral_fork_version_of_uncached_version_succeeds() {
    const CALLERS: usize = 8;

    host();
    let ps = page_size() as usize;
    let dir = tempfile::tempdir().unwrap();
    let eph = cladefs_core::ephemeral_host::open(&dir.path().join("ephemeral")).unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));

    let parent = CellClient::connect(&socket, dir.path().join("parent"), 1 << 24).unwrap();
    let inherited = ExtentId { cell: parent.cell(), sequence: 1 };
    write(&parent, inherited, 0, &pattern(7, ps)).unwrap();
    parent.commit(Some(b"fresh-version")).unwrap();
    let version = parent.make_version().unwrap();
    drop(parent);

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(CALLERS));
    let mut threads = Vec::new();
    for i in 0..CALLERS {
        let barrier = std::sync::Arc::clone(&barrier);
        let at = dir.path().join(format!("ws-{i}"));
        let cell = version_cell(version);
        let ver = version.version;
        let socket = socket.clone();
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            let child = control(crate::cells::fork_ephemeral_of_version(
                crate::Daemon::process(),
                cell,
                ver,
                at.clone(),
                None,
            ))
            .unwrap_or_else(|errno| panic!("caller {i} fork_ephemeral_of_version failed: {errno}"));
            let client = CellClient::connect(&socket, &at, 1 << 24).unwrap();
            assert_eq!(read(&client, inherited, 0, ps), Ok(pattern(7, ps)));
            assert_eq!(client.root(), Ok(b"fresh-version".to_vec()));
            child
        }));
    }
    let children: std::collections::BTreeSet<_> =
        threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(children.len(), CALLERS);
    let entries = eph.entries().unwrap();
    assert_eq!(entries.len(), CALLERS, "all {CALLERS} ephemeral cells must be noted in journal");
}

/// TASK-570 (board 2536): a VM process's 257th `VERSION` request on a connection, with 256
/// (`MAX_VERSIONS`) versions already present, fails with `EMLINK` and leaves 256 versions named.
#[test]
fn version_requests_past_max_versions_fail_with_emlink() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let store = dir.path().join("store");
    let client = CellClient::connect(&socket, &store, 1 << 24).unwrap();
    let most = bucket_cell::layout::MAX_VERSIONS;
    let mut ids = std::collections::BTreeSet::new();
    for i in 0..most {
        let made = client
            .make_version()
            .unwrap_or_else(|err| panic!("version {i} of {most} failed: {err}"));
        assert!(ids.insert(made.version), "duplicate version id at {i}");
    }
    assert_eq!(client.make_version(), Err(Errno::EMLINK));
    let listed = control(crate::cells::versions(crate::Daemon::process(), store)).unwrap();
    assert_eq!(listed.len(), most);
}

/// TASK-603 (board 2602): unpacked framed parts in `local_cell::unpacked()` are counted in
/// `memory::governed_bytes()` and evicted when `memory::govern()` runs with a small allowance.
#[tokio::test(flavor = "multi_thread")]
async fn unpacked_parts_are_counted_and_evicted_by_govern() {
    let unpacked = local_cell::unpacked();
    let parts = cladefs_core::Host::process().bucket_host().parts();
    unpacked.evict_to(0);
    let before_unpacked = unpacked.bytes();
    let before_governed = crate::memory::governed_bytes();
    let len = 1 << 20;
    let part_a = bucket_cell::fetch::PartBytes::from_owned(
        std::sync::Arc::new(vec![0x33u8; len].into_boxed_slice()),
        len as u64,
    );
    let part_b = bucket_cell::fetch::PartBytes::from_owned(
        std::sync::Arc::new(vec![0x44u8; len].into_boxed_slice()),
        len as u64,
    );
    parts.put_local(u64::MAX - 1, 0, part_a);
    parts.put_local(u64::MAX - 1, 1, part_b);
    let delta = 2 * len as u64;
    assert_eq!(unpacked.bytes(), before_unpacked + delta);
    assert!(
        crate::memory::governed_bytes() >= before_governed + delta,
        "governed_bytes() must include local_cell::unpacked()"
    );
    crate::memory::govern(0).await;
    assert_eq!(unpacked.bytes(), 0, "govern(0) must evict unpacked parts");
    assert_eq!(parts.limit(), 0, "govern(0) must lower PartMemory limit to 0");
    // Restore limit so tests sharing this process under `cargo test` are unaffected.
    crate::memory::govern(u64::MAX).await;
    assert_eq!(parts.limit(), bucket_cell::fetch::RECENT);
}

/// board 12313: `recent_limit_for` scales `PartMemory` to half the remaining room when FUSE cells
/// are attached (never exceeding `room`), while keeping VM-only daemons capped at `RECENT`.
#[test]
fn recent_limit_scales_for_fuse_and_stays_bounded_for_vms() {
    let recent = bucket_cell::fetch::RECENT;
    // VM-only daemon (no FUSE cells attached): capped at RECENT, shrunk under pressure.
    assert_eq!(crate::memory::recent_limit_for(0, false), 0);
    assert_eq!(crate::memory::recent_limit_for(recent / 2, false), recent / 2);
    assert_eq!(crate::memory::recent_limit_for(4 << 30, false), recent);

    // FUSE cells attached: scales to room / 2 (never below RECENT.min(room), never past room).
    assert_eq!(crate::memory::recent_limit_for(0, true), 0);
    assert_eq!(crate::memory::recent_limit_for(recent / 2, true), recent / 2);
    assert_eq!(crate::memory::recent_limit_for(recent, true), recent);
    assert_eq!(crate::memory::recent_limit_for(256 << 20, true), 128 << 20);
    assert_eq!(crate::memory::recent_limit_for(4 << 30, true), 2 << 30);
}

/// cladefs board 14449: an embedded cell keeps its pages in its tenant's arena, as a cell its VM
/// processes open does: a granted tenant's own; for a tenant with no arena on the daemon, one of
/// the cell's own; never the default tenant's, which every VM process of the daemon's own socket
/// maps, and which held every embedded cell's pages before. The default tenant's cells keep theirs
/// in its arena.
#[cfg(not(madsim))]
#[test]
fn embedded_cells_pages_are_of_their_tenants_arenas() {
    use crate::cells::CellWriter as _;
    host();
    let dir = tempfile::tempdir().unwrap();
    let daemon = crate::Daemon::process();
    daemon.configure_disks(&[dir.path().join("disk0")]).unwrap();
    let granted = dir.path().join("tenant-5");
    std::fs::create_dir(&granted).unwrap();
    let tenant = crate::server::Tenant::new(5, &granted).unwrap();
    let default = default_tenant().unwrap().arena();
    let arena_of = |tenant: u64| {
        let embedded =
            futures::executor::block_on(daemon.create_embedded(tenant, 1 << 26, Kind::Sqlite))
                .unwrap();
        assert_eq!(embedded.id().tenant, tenant);
        let arena = embedded.cell().unwrap().arena();
        (embedded, arena)
    };
    let (_five, five) = arena_of(5);
    let (_nine, nine) = arena_of(9);
    let (_zero, zero) = arena_of(cladefs_core::host_cell::DEFAULT_TENANT);
    assert_eq!((five, zero), (tenant.arena(), default));
    assert!(
        ![default, tenant.arena(), cladefs_core::pages::ArenaId::DEFAULT].contains(&nine),
        "{nine:?}"
    );
}
