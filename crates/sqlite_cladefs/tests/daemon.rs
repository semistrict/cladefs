// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! SQLite on a cell an extent daemon serves, in this process, as a host's would
//! serve it from its own (TASK-300): pages read in place in the daemon's window.

#![cfg(target_os = "linux")]

mod common;

use cladefs::cladefsd::control::proto::{ForkCellRequest, StatCellRequest, SyncCellRequest};
use cladefs::testing::control_path;
use futures::executor::block_on;
use rusqlite::{Connection, OpenFlags, params};
use sqlite_cladefs::{Database, Options, Registration, daemon, register};
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
        let database = daemon::open(&socket, &cell_dir, 1 << 30, 1 << 30, options).unwrap();
        let vfs = format!("cladefs-daemon-{name}-{round}");
        let registration = register(&vfs, database.clone()).unwrap();
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
        let before = database.fetched();
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
        // In place on both 4 KiB and 16 KiB hosts (4 KiB blocks composed into host pages as
        // needed).
        let fetched = database.fetched() - before;
        assert!(database.is_mapped(), "a window maps the database in place");
        assert!(fetched > 100, "{fetched} pages fetched");
        drop(connection);
        drop(registration);
        database.close().unwrap();
    }
    // Beside the daemon's cell, which works on behind its API (TASK-463).
    cladefs::testing::files_holding_beside(&cell_dir, &[&marker[..64]])
}

/// The database on the cell at `cell_dir`, mapped, registered as the VFS `vfs`, a connection to it
/// reading it in place, and the cell, to fork.
struct Mounted {
    cell: Arc<cladefs::CellClient>,
    database: Arc<Database>,
    _registration: Registration,
    connection: Connection,
}

impl Mounted {
    fn new(socket: &Path, cell_dir: &Path, vfs: &str) -> Self {
        let cell = Arc::new(cladefs::CellClient::connect(socket, cell_dir, 1 << 30).unwrap());
        let window = daemon::DaemonWindow::new(cell.clone(), 1 << 30).unwrap();
        let device = DeviceHolder::new(cladefs::DaemonDevice::new(cell.clone()).unwrap());
        let database = Database::open_mapped(device, Box::new(window), Options::default()).unwrap();
        let registration = register(vfs, database.clone()).unwrap();
        let connection =
            Connection::open_with_flags_and_vfs("app.db", OpenFlags::default(), vfs).unwrap();
        connection.execute_batch("PRAGMA mmap_size = 1073741824").unwrap();
        Self { cell, database, _registration: registration, connection }
    }

    /// Checks the device holds every extent the database names, those it references and those it
    /// has yet to free: so that none it frees, or failed to, is one the device never held, which
    /// it would try to free again at each commit; and closes the database.
    fn close(self) {
        let Self { cell, database, _registration, connection } = self;
        drop(connection);
        let device = database.device();
        let mut buffer = block_on(device.allocate_buffer(device.block_size() as usize));
        for extent in database.extents().unwrap() {
            block_on(device.read(extent, 0, buffer.as_mut()))
                .unwrap_or_else(|error| panic!("{extent:?} the database names: {error:#}"));
        }
        drop((cell, _registration));
        database.close().unwrap();
    }
}

/// A fork of a cell on the daemon, as of its last commit, holds the database as it was, inherited
/// with the cell's extents and pages, while the database on the cell is open, frees in its root
/// still to make; the two then change apart, the fork rewriting and so freeing all it inherited,
/// and each reads its own, in place, before and after opening again.
#[test]
fn a_fork_of_a_cell_and_the_cell_go_their_own_ways() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (sockets, socket) = serve();
    let control_socket = sockets.path().join("control.sock");
    let listener = UnixListener::bind(&control_socket).unwrap();
    let tenants = sockets.path().join("tenants");
    std::fs::create_dir(&tenants).unwrap();
    std::thread::spawn(move || cladefs::cladefsd::control::serve_control(listener, tenants));
    let control = cladefs::testing::Control::connect(&control_socket).unwrap();
    let (path, forked) = (dir.path().join("store"), dir.path().join("fork"));
    let parent = Mounted::new(&socket, &path, "cladefs-daemon-parent");
    let mut parent_rows = common::fill(&parent.connection);
    control
        .call(async |client| {
            client
                .fork_cell(ForkCellRequest {
                    from: control_path(&path),
                    to: control_path(&forked),
                    ..Default::default()
                })
                .await
        })
        .map(|response| cladefs::CellId { tenant: response.tenant, id: response.cell })
        .unwrap();
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
    let control = cladefs::testing::Control::connect(&control_socket).unwrap();

    let cell_dir = dir.path().join("store");
    let database = daemon::open(&socket, &cell_dir, 1 << 30, 1 << 30, Options::default()).unwrap();
    let _registration = register("cladefs-fenced", database.clone()).unwrap();
    let connection =
        Connection::open_with_flags_and_vfs("app.db", OpenFlags::default(), "cladefs-fenced")
            .unwrap();
    connection.execute_batch("CREATE TABLE t (k INTEGER PRIMARY KEY, v TEXT)").unwrap();
    connection.execute("INSERT INTO t VALUES (1, 'ours')", []).unwrap();
    control
        .call(async |client| {
            client.sync_cell(SyncCellRequest { path: control_path(&cell_dir) }).await
        })
        .map(drop)
        .unwrap();
    let ours = control
        .call(async |client| {
            client.stat_cell(StatCellRequest { path: control_path(&cell_dir) }).await
        })
        .map(|response| cladefs::CellId { tenant: response.tenant, id: response.cell })
        .unwrap();
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
    let fenced = control
        .call(async |client| {
            client.sync_cell(SyncCellRequest { path: control_path(&cell_dir) }).await
        })
        .map(drop)
        .unwrap_err();
    assert_eq!(fenced.errno(), Some(cladefs::Errno::EROFS), "{fenced:#}");
    let refused = connection.execute("INSERT INTO t VALUES (3, 'refused')", []).unwrap_err();
    assert_eq!(
        refused.sqlite_error_code(),
        Some(rusqlite::ErrorCode::SystemIoFailure),
        "{refused}"
    );
}

/// A SQLite cell on the daemon holds one database (board 5790), once many named files (TASK-551):
/// a second name is refused, and closed and opened again, the database opens by any name, as the
/// cell keeps none, reading and committing as it did.
#[test]
fn a_cell_holds_one_database_by_any_name() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (_sockets, socket) = serve();
    let cell_dir = dir.path().join("store");
    let database = daemon::open(&socket, &cell_dir, 1 << 30, 1 << 30, Options::default()).unwrap();
    let registration = register("cladefs-one", database.clone()).unwrap();
    let connection =
        Connection::open_with_flags_and_vfs("a.db", OpenFlags::default(), "cladefs-one").unwrap();
    connection.execute_batch("CREATE TABLE t (k INTEGER PRIMARY KEY, v INTEGER)").unwrap();
    connection.execute("INSERT INTO t VALUES (1, 7)", []).unwrap();
    let second = Connection::open_with_flags_and_vfs("b.db", OpenFlags::default(), "cladefs-one");
    assert_eq!(second.unwrap_err().sqlite_error_code(), Some(rusqlite::ErrorCode::CannotOpen));
    drop(connection);
    drop(registration);
    database.close().unwrap();
    drop(database);

    let database = daemon::open(&socket, &cell_dir, 1 << 30, 1 << 30, Options::default()).unwrap();
    let _registration = register("cladefs-one-again", database.clone()).unwrap();
    let connection =
        Connection::open_with_flags_and_vfs("b.db", OpenFlags::default(), "cladefs-one-again")
            .unwrap();
    let v: i64 = connection.query_row("SELECT v FROM t WHERE k = 1", [], |row| row.get(0)).unwrap();
    assert_eq!(v, 7);
    connection.execute("UPDATE t SET v = v + 1", []).unwrap();
}

/// Board 8551: an opening that fails once its window is made, the cell's root no block device's,
/// unmaps the window, however many times it fails: the opening's device held the cell, which the
/// window's memory must outlive, as the window went, and each failed opening left its window's
/// address space mapped for the process's life.
#[test]
fn failed_openings_unmap_their_windows() {
    /// Far larger than what else the process maps and unmaps meanwhile (some 100 MB): address
    /// space alone.
    const WINDOW: u64 = 4 << 30;
    /// The bytes of the process's address space: a window left mapped may merge with mappings
    /// beside it, so is found by what it adds, not as a mapping of its size.
    fn address_space() -> u64 {
        let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
        let pages: u64 = statm.split_whitespace().next().unwrap().parse().unwrap();
        pages * cladefs::pages::page_size()
    }
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (_sockets, socket) = serve();
    let cell_dir = dir.path().join("store");
    let database = daemon::open(&socket, &cell_dir, 1 << 30, WINDOW, Options::default()).unwrap();
    let open = address_space();
    database.close().unwrap();
    drop(database);
    let closed = address_space();
    assert!(open.saturating_sub(closed) > WINDOW / 2, "{open} bytes open, {closed} closed");
    {
        let opened = cladefs::DaemonConnection::connect(&socket)
            .and_then(|daemon| daemon.open_sqlite(&cell_dir, 1 << 30))
            .unwrap();
        let device = cladefs::DaemonDevice::new(Arc::new(opened)).unwrap();
        block_on(storage_device::Device::commit_root(&device, b"no block device")).unwrap();
        block_on(storage_device::Device::close(&device)).unwrap();
    }
    let before = address_space();
    for _ in 0..8 {
        let opened = daemon::open(&socket, &cell_dir, 1 << 30, WINDOW, Options::default());
        let error = format!("{:#}", opened.err().expect("opened a cell of no block device"));
        assert_eq!(error, "the device holds no block device");
    }
    let grown = address_space().saturating_sub(before);
    assert!(
        grown < WINDOW / 2,
        "{grown} bytes more address space: windows of failed openings left mapped"
    );
}
