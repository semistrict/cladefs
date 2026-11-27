// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Cells in directories of the host's: one to a directory (TASK-165.7), deleting
//! them, and taking a cell directory for one process at a time (TASK-44.1), and once in it.

use local_cell::{
    Description, Dir, File as _, HostDir, LocalCell, Opening, Options, Unmade, create_host,
    delete_host, host_bucket, host_cell, host_token, place_host,
};
use std::path::Path;
use storage_device::ExtentId;

const BLOCK: usize = 4096;
const CELL: bucket_cell::layout::CellId = bucket_cell::layout::test_cell(7);
const OPTIONS: Options = Options {
    max_extent: 64 << 20,
    segment_size: 64 << 10,
    segment_age: std::time::Duration::from_secs(3600),
    large_extent: 32 << 10,
    open_files: 2,
    held_most: 256 << 10,
};

fn create(path: &Path) -> local_cell::LocalCell<HostDir> {
    std::fs::create_dir(path).unwrap();
    let description = Description { block_size: BLOCK as u32, capacity: 1 << 20, cell: CELL };
    create_host(path, description, "memory:///host", bucket_cell::cell::Kind::Fxfs).unwrap();
    let cell = LocalCell::recover_unmade(HostDir::new(path).unwrap()).unwrap();
    let cell = cell.open(OPTIONS, Opening::new(CELL)).unwrap();
    cell.made().unwrap();
    cell
}

/// The cell at `path`, opened where the bucket holds nothing of its cell.
fn open_host(path: &Path) -> anyhow::Result<LocalCell<HostDir>> {
    LocalCell::open(HostDir::new(path)?, OPTIONS)
}

/// cladefs: a host's cell names its cell, and the bucket it is kept in (TASK-165.6), which its
/// description says, opened or not.
#[test]
fn a_cells_description_names_its_cell_and_bucket() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let cell = create(&path);
    assert_eq!(cell.bucket(), Some("memory:///host"));
    assert_eq!(host_bucket(&path).unwrap().as_deref(), Some("memory:///host"));
    assert_eq!(host_cell(&path).unwrap(), CELL);
    drop(cell);
    let opened = open_host(&path).unwrap();
    assert_eq!((opened.bucket(), opened.description().cell), (Some("memory:///host"), CELL));
}

/// TASK-648: an opening of a cell this process is closing waits for its close, then opens it,
/// rather than fail as at a second opening: a cell let go of closes on whatever thread drops it
/// last, its last commits made durable, seconds on a busy disk. Its close held where it says it is
/// closing; the opening held where it waits; then the close let go on.
#[test]
fn openings_wait_for_a_cell_closing() {
    let scenario = fail::FailScenario::setup();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let cell = create(&path);
    let (closing, closing_seen) = std::sync::mpsc::channel();
    let (go, gone_on) = std::sync::mpsc::channel::<()>();
    let gone_on = std::sync::Mutex::new(gone_on);
    // The first close alone held: the opening's own cell closes after it, unheld.
    let first = std::sync::atomic::AtomicBool::new(true);
    fail::cfg_callback("cell::closing", move || {
        if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
            closing.send(()).unwrap();
            gone_on.lock().unwrap().recv().unwrap();
        }
    })
    .unwrap();
    let (waiting, waiting_seen) = std::sync::mpsc::channel();
    let waiting = std::sync::Mutex::new(waiting);
    fail::cfg_callback("dir::claim_waits", move || waiting.lock().unwrap().send(()).unwrap())
        .unwrap();
    let closed = std::thread::spawn(move || drop(cell));
    let wait = std::time::Duration::from_secs(30);
    closing_seen.recv_timeout(wait).expect("the cell never said it was closing");
    let opening = std::thread::spawn({
        let path = path.clone();
        move || open_host(&path).map(drop).map_err(|err| format!("{err:#}"))
    });
    waiting_seen.recv_timeout(wait).expect("the opening never waited for the close");
    go.send(()).unwrap();
    closed.join().unwrap();
    assert_eq!(opening.join().unwrap(), Ok(()));
    scenario.teardown();
}

/// A cell is opened once in a process, as in all: a second opening, and a deletion, of one open
/// fail at once, rather than wait for the process itself (TASK-165.7).
#[tokio::test(flavor = "multi_thread")]
async fn cells_open_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let cell = host_fs::may_block(|| create(&path));
    let started = std::time::Instant::now();
    let again = host_fs::may_block(|| open_host(&path)).err().expect("opened twice");
    assert_eq!(format!("{again:#}"), "taking the cell directory: this process has the cell open");
    assert!(delete_host(&path, async |_| Ok(())).await.is_err(), "deleted an open cell");
    assert!(started.elapsed() < std::time::Duration::from_secs(1), "waited for itself");
    drop(cell);
    drop(host_fs::may_block(|| open_host(&path)).unwrap());
}

/// cladefs: a cell is no cell until its maker says it is whole: one whose maker stopped part way,
/// as an import a crash cut off, refuses to open, opened again or by another process (TASK-196);
/// once made, it opens.
#[tokio::test(flavor = "multi_thread")]
async fn cells_unmade_refuse_to_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let cell = host_fs::may_block(|| {
        std::fs::create_dir(&path).unwrap();
        let description = Description { block_size: BLOCK as u32, capacity: 1 << 20, cell: CELL };
        create_host(&path, description, "memory:///host", bucket_cell::cell::Kind::Fxfs).unwrap();
        let making = LocalCell::recover_unmade(HostDir::new(&path).unwrap()).unwrap();
        making.open(OPTIONS, Opening::new(CELL)).unwrap()
    });
    cell.write(ExtentId::new(CELL.id, 1), 0, &[1; BLOCK]).await.unwrap();
    cell.commit(None).await.unwrap();
    // Its maker gone, part way.
    drop(cell);
    host_fs::may_block(|| {
        let refused = open_host(&path).err().expect("an unmade cell opened");
        assert_eq!(refused.downcast_ref::<Unmade>(), Some(&Unmade), "{refused:#}");
        let making = LocalCell::recover_unmade(HostDir::new(&path).unwrap()).unwrap();
        let cell = making.open(OPTIONS, Opening::new(CELL)).unwrap();
        cell.made().unwrap();
        drop(cell);
        let cell = open_host(&path).unwrap();
        assert_eq!(cell.extent(ExtentId::new(CELL.id, 1)).unwrap(), Some((BLOCK as u64, false)));
    });
}

/// cladefs: a cell in a directory whose file system can't bypass the host's page cache, as the
/// failpoint makes every one, isn't made, its first file's opening (`unmade`) failing, naming it,
/// rather than the cell running in the host's page cache, a memory no budget counts (TASK-433.6).
#[test]
fn cells_whose_file_system_cant_bypass_the_page_cache_fail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    std::fs::create_dir(&path).unwrap();
    let description = Description { block_size: BLOCK as u32, capacity: 1 << 20, cell: CELL };
    let scenario = fail::FailScenario::setup();
    fail::cfg("direct::bypass", "return").unwrap();
    let err = create_host(&path, description, "memory:///host", bucket_cell::cell::Kind::Fxfs)
        .unwrap_err();
    scenario.teardown();
    let refused = format!(
        "{}: its file system can't bypass the host's page cache (O_DIRECT, F_NOCACHE): {}",
        path.join("unmade").display(),
        std::io::Error::from_raw_os_error(libc::EINVAL)
    );
    assert_eq!(format!("{err:#}"), refused);
}

/// cladefs: a cell's placement's token is its description's, placed durably, which opening it
/// takes its cell with (TASK-39.7, TASK-165.9); none, never placed.
#[test]
fn placements_name_their_token_in_the_description() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    drop(create(&path));
    assert_eq!(host_token(&path).unwrap(), None);
    place_host(&path, 7, true).unwrap();
    place_host(&path, 9, false).unwrap();
    assert_eq!(host_token(&path).unwrap(), Some(9));
    let cell = open_host(&path).unwrap();
    assert_eq!(cell.bucket(), Some("memory:///host"));
    let recovering = LocalCell::recover(HostDir::new(dir.path()).unwrap());
    assert!(recovering.is_err(), "the cell's parent opened");
}

/// Deleting a cell removes its directory, whole, once `before` deleted its cell elsewhere.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_cells() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let open_cell = host_fs::may_block(|| create(&path));
    open_cell.write(ExtentId::new(CELL.id, 1), 0, &[1; BLOCK]).await.unwrap();
    open_cell.commit(None).await.unwrap();
    drop(open_cell);
    let deleted = std::cell::Cell::new(None);
    delete_host(&path, async |cell| Ok(deleted.set(Some(cell)))).await.unwrap();
    assert_eq!((deleted.get(), path.exists()), (Some(CELL), false));
    assert!(host_fs::may_block(|| open_host(&path)).is_err(), "the deleted cell opened");
}

/// A file removed is gone.
#[test]
fn removed_files_are_gone() {
    let dir = tempfile::tempdir().unwrap();
    let host = HostDir::new(dir.path()).unwrap();
    host.create("file", false).unwrap().write_at(b"data", 0).unwrap();
    assert_eq!(host.list("").unwrap(), ["file"]);
    host.remove("file").unwrap();
    assert_eq!(host.list("").unwrap(), Vec::<String>::new());
}
