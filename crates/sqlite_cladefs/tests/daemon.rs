// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! SQLite on a file set on a cell an extent daemon serves, in this process, as a host's would
//! serve it from its own (TASK-300): pages read in place in the daemon's window.

#![cfg(target_os = "linux")]

mod common;

use futures::executor::block_on;
use rusqlite::{Connection, OpenFlags, params};
use sqlite_cladefs::{FileSet, Options, Registration, daemon, register};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use storage_device::DeviceHolder;

/// A daemon on a thread of this process, as a host of the multi-node system runs it: a bucket in
/// its memory, placement 1. Its socket is in a directory of its own in the system's temporary
/// directory, which lives as long as the one returned: a socket's path is short (SUN_LEN), and one
/// under the target directory of a checkout deep in a host's scratch disk was not (TASK-344).
fn serve() -> (tempfile::TempDir, PathBuf) {
    serve_from("memory:///sqlite-cladefs")
}

/// A daemon as [`serve`] runs it, of the bucket at `url`.
fn serve_from(url: &str) -> (tempfile::TempDir, PathBuf) {
    // SAFETY: before this process starts any other thread.
    unsafe {
        std::env::set_var("CLADEFSD_BUCKET", url);
        std::env::set_var("CLADEFSD_TOKEN", "1");
    }
    let sockets = tempfile::tempdir().unwrap();
    let socket = sockets.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || cladefs::cladefsd::serve(listener));
    (sockets, socket)
}

#[test]
fn a_database_on_the_daemon_reads_in_place_and_survives_reopening() {
    reads_in_place_and_survives_reopening("plain");
}

/// TASK-411.7 (TASK-411's AC #3, SQLite cells, Linux): a database on a cell of an encrypted cell
/// the daemon serves, its tenant's KEK in the host's KMS (the default tenant's, whose cells the
/// daemon makes for cells no tenant's grant names), reads in place and survives reopening as one
/// in the clear does; and no file of the cell directory holds a row of noise it wrote in the
/// clear.
#[test]
fn an_encrypted_database_on_the_daemon_reads_in_place_and_survives_reopening() {
    let keys = tempfile::tempdir().unwrap();
    let kms = keys.path().join("kms");
    extent_keys::kms::LocalKms::open(&kms).unwrap().create(0).unwrap();
    // The daemon's KMS, as a host's is named: SAFETY: before this process starts any other thread.
    unsafe { std::env::set_var("CLADEFS_KMS", format!("local://{}", kms.display())) };
    assert_eq!(reads_in_place_and_survives_reopening("encrypted"), Vec::<String>::new());
}

/// Data that compresses not, which a file holding it in the clear holds as it is.
fn noise(len: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

/// A database on a cell the daemon serves, written, read in place, closed and opened again, its
/// VFSs named after `name`: returns the files of the cell directory that hold a row of
/// [`noise`] it wrote in the clear.
fn reads_in_place_and_survives_reopening(name: &str) -> Vec<String> {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (_sockets, socket) = serve();
    let cell_dir = dir.path().join("store");
    let marker = noise(4096);

    for round in 0..2i64 {
        let options = Options::default();
        let file_set = daemon::open(&socket, &cell_dir, 1 << 30, 1 << 30, options).unwrap();
        let vfs = format!("cladefs-daemon-{name}-{round}");
        let registration = register(&vfs, file_set.clone()).unwrap();
        let connection =
            Connection::open_with_flags_and_vfs("app.db", OpenFlags::default(), vfs.as_str())
                .unwrap();
        connection.execute_batch("PRAGMA mmap_size = 1073741824; CREATE TABLE IF NOT EXISTS t (k INTEGER PRIMARY KEY, v BLOB)").unwrap();
        let transaction = connection.unchecked_transaction().unwrap();
        for k in 0..2000 {
            transaction
                .execute(
                    "INSERT OR REPLACE INTO t VALUES (?1, ?2)",
                    params![k, vec![round as u8; 300]],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
        for k in (0..2000).step_by(7) {
            connection
                .execute("UPDATE t SET v = ?2 WHERE k = ?1", params![k, vec![9u8; 300]])
                .unwrap();
        }
        connection
            .execute("INSERT OR REPLACE INTO t VALUES (?1, ?2)", params![5000, marker])
            .unwrap();
        let before = file_set.fetched();
        let check: String =
            connection.query_row("PRAGMA integrity_check", [], |row| row.get(0)).unwrap();
        assert_eq!(check, "ok");
        let (count, nines): (i64, i64) = connection
            .query_row(
                "SELECT count(*), sum(v = ?1) FROM t WHERE k < 5000",
                params![vec![9u8; 300]],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((count, nines), (2000, 286));
        let read: Vec<u8> =
            connection.query_row("SELECT v FROM t WHERE k = 5000", [], |row| row.get(0)).unwrap();
        assert!(read == marker, "the row of noise reads back otherwise");
        // In place where the host maps the cell's blocks into a window; a host of larger pages
        // has none, its reads copied (sqlite_cladefs::daemon, board 5232).
        let fetched = file_set.fetched() - before;
        let in_place = file_set.is_mapped();
        assert_eq!(in_place, cladefs::pages::page_size() <= 4096, "a window where it fits");
        assert!(if in_place { fetched > 100 } else { fetched == 0 }, "{fetched} pages fetched");
        drop(connection);
        drop(registration);
        file_set.close().unwrap();
    }
    // Beside the daemon's cell, which works on behind its API (TASK-463).
    cladefs::testing::files_holding_beside(&cell_dir, &[&marker[..64]])
}

/// A file set on the cell at `cell_dir`, mapped, registered as the VFS `vfs`, a connection to its
/// database reading it in place, and the cell, to fork.
struct Mounted {
    cell: Arc<cladefs::CellClient>,
    file_set: Arc<FileSet>,
    _registration: Registration,
    connection: Connection,
}

impl Mounted {
    fn new(socket: &Path, cell_dir: &Path, vfs: &str) -> Self {
        let cell = Arc::new(cladefs::CellClient::connect(socket, cell_dir, 1 << 30).unwrap());
        let window = daemon::DaemonWindow::new(cell.clone(), 1 << 30).unwrap();
        let device = DeviceHolder::new(cladefs::DaemonDevice::new(cell.clone()).unwrap());
        let file_set = FileSet::open_mapped(device, Box::new(window), Options::default()).unwrap();
        let registration = register(vfs, file_set.clone()).unwrap();
        let connection =
            Connection::open_with_flags_and_vfs("app.db", OpenFlags::default(), vfs).unwrap();
        connection.execute_batch("PRAGMA mmap_size = 1073741824").unwrap();
        Self { cell, file_set, _registration: registration, connection }
    }

    /// Checks the device holds every extent the file set names, those it references and those it
    /// has yet to free: so that none it frees, or failed to, is one the device never held, which
    /// it would try to free again at each commit; and closes the file set.
    fn close(self) {
        let Self { cell, file_set, _registration, connection } = self;
        drop(connection);
        let device = file_set.device();
        let mut buffer = block_on(device.allocate_buffer(device.block_size() as usize));
        for extent in file_set.extents().unwrap() {
            block_on(device.read(extent, 0, buffer.as_mut()))
                .unwrap_or_else(|error| panic!("{extent:?} the file set names: {error:#}"));
        }
        drop((cell, _registration));
        file_set.close().unwrap();
    }
}

/// A fork of a cell on the daemon, as of its last commit, holds the database as it was, inherited
/// with the cell's extents and pages, while the file set on the cell is open, frees in its root
/// still to make; the two then change apart, the fork rewriting and so freeing all it inherited,
/// and each reads its own, in place, before and after opening again.
#[test]
fn a_fork_of_a_cell_and_the_cell_go_their_own_ways() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (_sockets, socket) = serve();
    let (path, forked) = (dir.path().join("store"), dir.path().join("fork"));
    let parent = Mounted::new(&socket, &path, "cladefs-daemon-parent");
    let mut parent_rows = common::fill(&parent.connection);
    parent.cell.fork(&forked).unwrap();
    let fork = Mounted::new(&socket, &forked, "cladefs-daemon-fork");
    let mut fork_rows = parent_rows.clone();
    common::check(&fork.connection, &fork_rows);

    common::change_parent(&parent.connection, &mut parent_rows);
    common::change_fork(&fork.connection, &mut fork_rows);
    common::check(&parent.connection, &parent_rows);
    common::check(&fork.connection, &fork_rows);
    fork.close();
    parent.close();

    for (round, (path, rows)) in [(&path, parent_rows), (&forked, fork_rows)].iter().enumerate() {
        let mounted = Mounted::new(&socket, path, &format!("cladefs-daemon-reopened-{round}"));
        common::check(&mounted.connection, rows);
        mounted.close();
    }
}

/// TASK-364: a SQLite cell is fenced as any cell is. Another host, of a later
/// placement, takes the cell, and the cell with it: the first host's commit, durable on
/// its disk alone, is not in the bucket, its wait for it failing; its commits after it fail.
#[test]
fn a_database_is_fenced_with_its_cell() {
    use bucket_cell::cell;
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let bucket = dir.path().join("bucket");
    std::fs::create_dir(&bucket).unwrap();
    let url = format!("file://{}", bucket.display());
    let (sockets, socket) = serve_from(&url);
    let control_socket = sockets.path().join("control.sock");
    let listener = UnixListener::bind(&control_socket).unwrap();
    let tenants = sockets.path().join("tenants");
    std::fs::create_dir(&tenants).unwrap();
    std::thread::spawn(move || cladefs::cladefsd::control::serve_control(listener, tenants));
    let control = cladefs::cladefsd::control::Client::connect(&control_socket).unwrap();

    let cell_dir = dir.path().join("store");
    let file_set = daemon::open(&socket, &cell_dir, 1 << 30, 1 << 30, Options::default()).unwrap();
    let _registration = register("cladefs-fenced", file_set.clone()).unwrap();
    let connection =
        Connection::open_with_flags_and_vfs("app.db", OpenFlags::default(), "cladefs-fenced")
            .unwrap();
    connection.execute_batch("CREATE TABLE t (k INTEGER PRIMARY KEY, v TEXT)").unwrap();
    connection.execute("INSERT INTO t VALUES (1, 'ours')", []).unwrap();
    control.sync_cell(&cell_dir).unwrap();
    let ours = control.cell(&cell_dir).unwrap();
    let objects = bucket_cell::object_bucket::ObjectBucket::from_url(&url).unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let held = runtime.block_on(bucket_cell::head::get(&objects, ours)).unwrap().unwrap();
    assert_eq!(held.kind, Some(cell::Kind::Sqlite));
    assert_eq!((held.epoch, held.token), (1, 1));

    // Another host's, placed later: its writer fences the cell (TASK-371), unwrapping the cell's
    // KEK through the host's KMS, the daemon's (CLADEFS_KMS above), as the tests' own KMS has none
    // of it.
    let objects = std::sync::Arc::new(objects);
    runtime.block_on(async {
        let kms = cladefs::bucket::kms().unwrap();
        let writer = bucket_cell::Writer::open_under(objects.clone(), ours, (0x22, 2), &kms);
        drop(writer.await.unwrap());
    });
    connection.execute("INSERT INTO t VALUES (2, 'cut off')", []).unwrap();
    let fenced = control.sync_cell(&cell_dir).unwrap_err();
    assert_eq!(fenced.errno(), Some(cladefs::Errno::EROFS), "{fenced:#}");
    let refused = connection.execute("INSERT INTO t VALUES (3, 'refused')", []).unwrap_err();
    assert_eq!(
        refused.sqlite_error_code(),
        Some(rusqlite::ErrorCode::SystemIoFailure),
        "{refused}"
    );
}

/// TASK-551: a SQLite cell on the daemon holding more databases than its root holds the names of
/// (80 of 30-byte names, past the daemon's 4 KiB root) commits each, and every one reads back
/// after the file set is closed and opened again: the root lists its files in an extent of their
/// own, where it once failed the file set, every database's connections getting SQLITE_IOERR.
#[test]
fn a_cell_of_many_databases_commits_each_and_opens_again() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (_sockets, socket) = serve();
    let cell_dir = dir.path().join("store");
    let names: Vec<String> = (0..80).map(|n| format!("{n:0>27}.db")).collect();
    assert!(names.iter().all(|name| name.len() == 30));
    let file_set = daemon::open(&socket, &cell_dir, 1 << 30, 1 << 30, Options::default()).unwrap();
    let registration = register("cladefs-many", file_set.clone()).unwrap();
    for (n, name) in names.iter().enumerate() {
        let connection =
            Connection::open_with_flags_and_vfs(name, OpenFlags::default(), "cladefs-many")
                .unwrap();
        connection.execute_batch("CREATE TABLE t (k INTEGER PRIMARY KEY, v INTEGER)").unwrap();
        connection.execute("INSERT INTO t VALUES (1, ?1)", params![n as i64]).unwrap();
    }
    assert_eq!(file_set.files().unwrap().len(), names.len());
    drop(registration);
    file_set.close().unwrap();
    drop(file_set);

    let file_set = daemon::open(&socket, &cell_dir, 1 << 30, 1 << 30, Options::default()).unwrap();
    let _registration = register("cladefs-many-again", file_set.clone()).unwrap();
    for (n, name) in names.iter().enumerate() {
        let connection =
            Connection::open_with_flags_and_vfs(name, OpenFlags::default(), "cladefs-many-again")
                .unwrap();
        let v: i64 =
            connection.query_row("SELECT v FROM t WHERE k = 1", [], |row| row.get(0)).unwrap();
        assert_eq!(v, n as i64, "{name}");
        // And commits again, its root still listing its files.
        connection.execute("UPDATE t SET v = v + 1", []).unwrap();
    }
}
