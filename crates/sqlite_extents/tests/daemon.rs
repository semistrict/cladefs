// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! SQLite on a file set on a store an extent daemon serves, in this process, as a host's would
//! serve it from its own (TASK-300): pages read in place in the daemon's window.

#![cfg(target_os = "linux")]

mod common;

use futures::executor::block_on;
use rusqlite::{Connection, OpenFlags, params};
use sqlite_extents::{FileSet, Options, Registration, daemon, register};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use storage_device::DeviceHolder;

/// A daemon on a thread of this process, as a host of the multi-node system runs it: a bucket in
/// its memory, placement 1. Its socket is in a directory of its own in the system's temporary
/// directory, which lives as long as the one returned: a socket's path is short (SUN_LEN), and one
/// under the target directory of a checkout deep in a host's scratch disk was not (TASK-344).
fn serve() -> (tempfile::TempDir, PathBuf) {
    serve_from("memory:///sqlite-extents")
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

/// TASK-411.7 (TASK-411's AC #3, SQLite vdisks, Linux): a database on a store of an encrypted cell
/// the daemon serves, its tenant's KEK in the host's KMS (the default tenant's, whose cells the
/// daemon makes for stores no tenant's grant names), reads in place and survives reopening as one
/// in the clear does; and no file of the store's directory holds a row of noise it wrote in the
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

/// A database on a store the daemon serves, written, read in place, closed and opened again, its
/// VFSs named after `name`: returns the files of the store's directory that hold a row of
/// [`noise`] it wrote in the clear.
fn reads_in_place_and_survives_reopening(name: &str) -> Vec<String> {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (_sockets, socket) = serve();
    let store = dir.path().join("store");
    let marker = noise(4096);

    for round in 0..2i64 {
        let file_set = daemon::open(&socket, &store, 1 << 30, 1 << 30, Options::default()).unwrap();
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
        assert!(file_set.fetched() > before + 100, "{} pages fetched", file_set.fetched() - before);
        drop(connection);
        drop(registration);
        file_set.close().unwrap();
    }
    // Beside the daemon's store, which works on behind its API (TASK-463).
    cladefs::testing::files_holding_beside(&store, &[&marker[..64]])
}

/// A file set on the store at `store`, mapped, registered as the VFS `vfs`, a connection to its
/// database reading it in place, and the store, to fork.
struct Mounted {
    store: Arc<cladefs::DaemonExtentStore>,
    file_set: Arc<FileSet>,
    _registration: Registration,
    connection: Connection,
}

impl Mounted {
    fn new(socket: &Path, store: &Path, vfs: &str) -> Self {
        let store = Arc::new(cladefs::DaemonExtentStore::connect(socket, store, 1 << 30).unwrap());
        let window = daemon::DaemonWindow::new(store.clone(), 1 << 30).unwrap();
        let device = DeviceHolder::new(cladefs::DaemonDevice::new(store.clone()).unwrap());
        let file_set = FileSet::open_mapped(device, Box::new(window), Options::default()).unwrap();
        let registration = register(vfs, file_set.clone()).unwrap();
        let connection =
            Connection::open_with_flags_and_vfs("app.db", OpenFlags::default(), vfs).unwrap();
        connection.execute_batch("PRAGMA mmap_size = 1073741824").unwrap();
        Self { store, file_set, _registration: registration, connection }
    }

    /// Checks the device holds every extent the file set names, those it references and those it
    /// has yet to free: so that none it frees, or failed to, is one the device never held, which
    /// it would try to free again at each commit; and closes the file set.
    fn close(self) {
        let Self { store, file_set, _registration, connection } = self;
        drop(connection);
        let device = file_set.device();
        let mut buffer = block_on(device.allocate_buffer(device.block_size() as usize));
        for extent in file_set.extents().unwrap() {
            block_on(device.read(extent, 0, buffer.as_mut()))
                .unwrap_or_else(|error| panic!("{extent:?} the file set names: {error:#}"));
        }
        drop((store, _registration));
        file_set.close().unwrap();
    }
}

/// A fork of a store on the daemon, as of its last commit, holds the database as it was, inherited
/// with the store's extents and pages, while the file set on the store is open, frees in its root
/// still to make; the two then change apart, the fork rewriting and so freeing all it inherited,
/// and each reads its own, in place, before and after opening again.
#[test]
fn a_fork_of_a_store_and_the_store_go_their_own_ways() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (_sockets, socket) = serve();
    let (path, forked) = (dir.path().join("store"), dir.path().join("fork"));
    let parent = Mounted::new(&socket, &path, "cladefs-daemon-parent");
    let mut parent_rows = common::fill(&parent.connection);
    parent.store.fork(&forked).unwrap();
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

/// TASK-364: a SQLite vdisk is a vdisk of its cell, fenced with it. Another host, of a later
/// placement, takes the cell, and the vdisk's branch with it: the first host's commit, durable on
/// its disk alone, is not in the bucket, its wait for it failing; its commits after it fail.
#[test]
fn a_database_is_fenced_with_its_cell() {
    use extent_bucket::cell;
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

    let store = dir.path().join("store");
    let file_set = daemon::open(&socket, &store, 1 << 30, 1 << 30, Options::default()).unwrap();
    let _registration = register("cladefs-fenced", file_set.clone()).unwrap();
    let connection =
        Connection::open_with_flags_and_vfs("app.db", OpenFlags::default(), "cladefs-fenced")
            .unwrap();
    connection.execute_batch("CREATE TABLE t (k INTEGER PRIMARY KEY, v TEXT)").unwrap();
    connection.execute("INSERT INTO t VALUES (1, 'ours')", []).unwrap();
    control.sync_store(&store).unwrap();
    let ours = control.branch(&store).unwrap();
    let objects = extent_bucket::object_bucket::ObjectBucket::from_url(&url).unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let held = runtime.block_on(cell::read(&objects, ours.cell)).unwrap().unwrap();
    let named = cell::Vdisk { branch: ours.id, kind: cell::Kind::Sqlite };
    assert_eq!(held.vdisks, std::collections::BTreeMap::from([("store".to_owned(), named)]));
    assert_eq!((held.epoch, held.token), (1, 1));

    // Another host's, placed later.
    let objects = std::sync::Arc::new(objects);
    runtime.block_on(async {
        let (held, _) = cell::hold(&*objects, ours.cell, (2, 2), None, None).await.unwrap();
        // Its writer fences the branch (TASK-371), unwrapping the cell's KEK through the host's
        // KMS, the daemon's (CLADEFS_KMS above), as the tests' own KMS has none of it.
        let kms = cladefs::bucket::kms().unwrap();
        let writer =
            extent_bucket::Writer::open_under(objects.clone(), ours, (0x22, held.epoch), &kms);
        drop(writer.await.unwrap());
    });
    connection.execute("INSERT INTO t VALUES (2, 'cut off')", []).unwrap();
    let fenced = control.sync_store(&store).unwrap_err();
    assert_eq!(fenced.errno(), Some(cladefs::Errno::EROFS), "{fenced:#}");
    let refused = connection.execute("INSERT INTO t VALUES (3, 'refused')", []).unwrap_err();
    assert_eq!(
        refused.sqlite_error_code(),
        Some(rusqlite::ErrorCode::SystemIoFailure),
        "{refused}"
    );
}
