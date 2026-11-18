// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! SQLite on file sets on fake devices (TASK-296).

mod common;

use extent_files::testing::CopyingWindow;
use rand::rngs::StdRng;
use rand::{RngExt as _, SeedableRng as _};
use rusqlite::{Connection, OpenFlags, params};
use sqlite_extents::{FileSet, Options, Registration, register};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use storage_device::DeviceHolder;
use storage_device::fake_device::{Crashes, FakeDevice, Op};

const BLOCK: u32 = 4096;
/// A window's bytes: four regions of 16 MiB, as the default options cut it.
const WINDOW: u64 = 64 << 20;

/// A device of 1 GiB at most, as awkward as a store.
fn device() -> FakeDevice {
    FakeDevice::new(1 << 18, BLOCK)
}

/// How a test's file sets read: from the device, or from a window, SQLite's memory-mapped reads
/// reading it in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Device,
    Window,
}

/// Runs `test` in each mode.
fn both(test: impl Fn(Mode)) {
    test(Mode::Device);
    test(Mode::Window);
}

/// A file set on `device`, registered as a VFS of a name of its own.
struct Mounted {
    file_set: Arc<FileSet>,
    vfs: String,
    mode: Mode,
    _registration: Registration,
}

impl Mounted {
    fn new(device: DeviceHolder, mode: Mode) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let vfs = format!("cladefs-test-{}", NEXT.fetch_add(1, Ordering::Relaxed));
        let options = Options::default();
        let file_set = match mode {
            Mode::Device => FileSet::open(device, options),
            Mode::Window => {
                FileSet::open_mapped(device, Box::new(CopyingWindow::new(WINDOW, BLOCK)), options)
            }
        };
        let file_set = file_set.expect("opening the file set");
        let registration = register(&vfs, file_set.clone()).expect("registering");
        Self { file_set, vfs, mode, _registration: registration }
    }

    fn connect(&self, name: &str) -> Connection {
        let connection =
            Connection::open_with_flags_and_vfs(name, OpenFlags::default(), self.vfs.as_str())
                .expect("opening the database");
        if self.mode == Mode::Window {
            connection.execute_batch("PRAGMA mmap_size = 1073741824").unwrap();
        }
        connection
    }

    /// The device as a crash that loses nothing leaves it.
    fn snapshot(&self) -> DeviceHolder {
        self.file_set.device().snapshot().expect("a snapshot")
    }

    /// The file set closed, as a cell's is at its end, and its device as closing left it.
    fn close(self) -> DeviceHolder {
        self.file_set.close().expect("closing the file set");
        self.snapshot()
    }

    /// The bytes the extents the device holds take.
    fn held_bytes(&self) -> u64 {
        let device = self.file_set.device();
        device.extents().iter().map(|extent| device.extent_len(*extent).unwrap()).sum()
    }

    /// Checks that the device holds just what the file set's files reference.
    fn check_extents(&self) {
        assert_eq!(self.file_set.device().extents(), self.file_set.extents().unwrap());
    }
}

fn integrity(connection: &Connection) -> String {
    connection.query_row("PRAGMA integrity_check", [], |row| row.get(0)).unwrap()
}

fn rows(connection: &Connection) -> BTreeMap<i64, Vec<u8>> {
    let mut statement = connection.prepare("SELECT k, v FROM t ORDER BY k").unwrap();
    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
    rows.map(Result::unwrap).collect()
}

#[test]
fn a_database_reads_back_once_opened_again() {
    both(|mode| {
        let mounted = Mounted::new(DeviceHolder::new(device()), mode);
        let connection = mounted.connect("main.db");
        connection.execute_batch("CREATE TABLE t (k INTEGER PRIMARY KEY, v BLOB)").unwrap();
        for round in 0..20i64 {
            let transaction = connection.unchecked_transaction().unwrap();
            for k in 0..50 {
                let v = vec![(round + k) as u8; 300 + k as usize];
                transaction
                    .execute("INSERT OR REPLACE INTO t VALUES (?1, ?2)", params![k + round * 7, v])
                    .unwrap();
            }
            transaction.commit().unwrap();
        }
        let expected = rows(&connection);
        assert_eq!(expected.len(), 183);
        drop(connection);
        mounted.check_extents();

        let reopened = Mounted::new(mounted.snapshot(), mode);
        assert_eq!(reopened.file_set.files().unwrap(), vec!["main.db".to_owned()]);
        let connection = reopened.connect("main.db");
        assert_eq!(integrity(&connection), "ok");
        assert_eq!(rows(&connection), expected);
        reopened.check_extents();
    });
}

/// A fork of a file set's device, as of its last commit, holds the file set as it was; the fork and
/// the file set then change apart, each freeing what it no longer references, the fork what it
/// inherited, and each holds just what it references, before and after opening again.
#[test]
fn forks_and_their_parents_go_their_own_ways() {
    both(|mode| {
        let parent = Mounted::new(DeviceHolder::new(device()), mode);
        let connection = parent.connect("main.db");
        let mut parent_rows = common::fill(&connection);
        let fork = Mounted::new(parent.file_set.device().fork(7).expect("a fork"), mode);
        let forked = fork.connect("main.db");
        let mut fork_rows = parent_rows.clone();
        common::check(&forked, &fork_rows);

        common::change_parent(&connection, &mut parent_rows);
        common::change_fork(&forked, &mut fork_rows);
        common::check(&connection, &parent_rows);
        common::check(&forked, &fork_rows);
        drop((connection, forked));
        parent.check_extents();
        fork.check_extents();

        for (mounted, rows) in [(parent, parent_rows), (fork, fork_rows)] {
            let reopened = Mounted::new(mounted.snapshot(), mode);
            common::check(&reopened.connect("main.db"), &rows);
            reopened.check_extents();
        }
    });
}

#[test]
fn several_databases_share_a_file_set() {
    both(|mode| {
        let mounted = Mounted::new(DeviceHolder::new(device()), mode);
        for (name, count) in [("a.db", 10), ("b.db", 400), ("c.db", 0)] {
            let connection = mounted.connect(name);
            connection.execute_batch("CREATE TABLE t (k INTEGER PRIMARY KEY, v BLOB)").unwrap();
            for k in 0..count {
                connection
                    .execute("INSERT INTO t VALUES (?1, ?2)", params![k, name.as_bytes()])
                    .unwrap();
            }
        }
        // One deleted.
        let connection = mounted.connect("a.db");
        connection.execute_batch("ATTACH 'c.db' AS c; DETACH c").unwrap();
        drop(connection);
        mounted.file_set.delete("c.db").unwrap();
        mounted.check_extents();

        let reopened = Mounted::new(mounted.snapshot(), mode);
        assert_eq!(reopened.file_set.files().unwrap(), vec!["a.db".to_owned(), "b.db".to_owned()]);
        for (name, count) in [("a.db", 10), ("b.db", 400)] {
            let connection = reopened.connect(name);
            assert_eq!(rows(&connection).len(), count);
            assert_eq!(rows(&connection)[&3], name.as_bytes());
        }
        let missing = Connection::open_with_flags_and_vfs(
            "c.db",
            OpenFlags::SQLITE_OPEN_READ_WRITE,
            reopened.vfs.as_str(),
        );
        assert_eq!(missing.unwrap_err().sqlite_error_code(), Some(rusqlite::ErrorCode::CannotOpen));
    });
}

/// A transaction committed is seen by the other connections, and one in progress isn't.
#[test]
fn connections_see_committed_transactions() {
    both(|mode| {
        let mounted = Mounted::new(DeviceHolder::new(device()), mode);
        let writer = mounted.connect("main.db");
        let reader = mounted.connect("main.db");
        writer
            .execute_batch(
                "CREATE TABLE t (k INTEGER PRIMARY KEY, v BLOB); INSERT INTO t VALUES (1, x'01')",
            )
            .unwrap();
        writer.execute_batch("BEGIN; INSERT INTO t VALUES (2, x'02')").unwrap();
        assert_eq!(rows(&reader).len(), 1);
        writer.execute_batch("COMMIT").unwrap();
        assert_eq!(rows(&reader).len(), 2);
    });
}

/// A crash at every barrier of a run of transactions leaves the database as of the
/// transactions committed before it, whole, and the device holding what it references alone.
#[test]
fn a_crash_leaves_the_transactions_committed() {
    both(|mode| {
        for seed in 0..4 {
            let crashes = Crashes::new(seed, 1);
            let mounted =
                Mounted::new(DeviceHolder::new(device().record_crashes(crashes.clone())), mode);
            let connection = mounted.connect("main.db");
            connection
                .execute_batch(
                    "PRAGMA cache_size = 20; CREATE TABLE t (k INTEGER PRIMARY KEY, v BLOB)",
                )
                .unwrap();
            let mut model = BTreeMap::new();
            let mut committed = vec![model.clone()];
            crashes.mark(0);
            for round in 0..40i64 {
                let transaction = connection.unchecked_transaction().unwrap();
                // Inserts, updates and deletes, over enough rows for trees of two levels and more
                // than SQLite's cache holds.
                for k in 0..60 {
                    let key = (k * 37 + round * 11) % 700;
                    let v = vec![round as u8; 200 + (k as usize * 13) % 900];
                    match (k + round) % 5 {
                        0 => {
                            transaction
                                .execute("DELETE FROM t WHERE k = ?1", params![key])
                                .unwrap();
                            model.remove(&key);
                        }
                        _ => {
                            transaction
                                .execute(
                                    "INSERT OR REPLACE INTO t VALUES (?1, ?2)",
                                    params![key, v],
                                )
                                .unwrap();
                            model.insert(key, v);
                        }
                    }
                }
                transaction.commit().unwrap();
                committed.push(model.clone());
                crashes.mark(round as u64 + 1);
            }
            drop(connection);

            let mut recovered = 0;
            for (mark, device) in crashes.take() {
                let reopened = Mounted::new(DeviceHolder::new(device), mode);
                reopened.check_extents();
                let connection = reopened.connect("main.db");
                assert_eq!(integrity(&connection), "ok", "seed {seed}, mark {mark}");
                assert_eq!(rows(&connection), committed[mark as usize], "seed {seed}, mark {mark}");
                recovered += 1;
            }
            assert_eq!(recovered, 40, "seed {seed}");
        }
    });
}

/// What the device holds stays within a bound of the database, however often its pages are
/// written again.
#[test]
fn space_stays_bounded_under_updates() {
    both(|mode| {
        let mounted = Mounted::new(DeviceHolder::new(FakeDevice::new(1 << 18, BLOCK).calm()), mode);
        let connection = mounted.connect("main.db");
        connection.execute_batch("CREATE TABLE t (k INTEGER PRIMARY KEY, v BLOB)").unwrap();
        let transaction = connection.unchecked_transaction().unwrap();
        for k in 0..2000i64 {
            transaction
                .execute("INSERT INTO t VALUES (?1, ?2)", params![k, vec![1u8; 1000]])
                .unwrap();
        }
        transaction.commit().unwrap();
        for round in 0..3000i64 {
            let key = (round * 7919) % 2000;
            connection
                .execute("UPDATE t SET v = ?2 WHERE k = ?1", params![key, vec![round as u8; 1000]])
                .unwrap();
        }
        let size: i64 = connection
            .query_row(
                "SELECT page_count * page_size FROM pragma_page_count, pragma_page_size",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let (size, held) = (size as u64, mounted.held_bytes());
        println!("database {size} bytes, device {held} bytes");
        assert!(held <= size * 3, "the device holds {held} bytes for a database of {size}");
        mounted.check_extents();
    });
}

/// A commit whose root fails fails the file set; opened again, it holds the database as of the
/// commit before, and frees what the failed one wrote.
#[test]
fn a_failed_commit_leaves_nothing_behind() {
    both(|mode| {
        let mut device = device();
        let failing = Arc::new(AtomicBool::new(false));
        let fail = failing.clone();
        device.set_op_callback(move |op| match op {
            Op::Flush | Op::PostBarrier if fail.load(Ordering::Relaxed) => {
                Err(anyhow::anyhow!("the device fails"))
            }
            _ => Ok(()),
        });
        let mounted = Mounted::new(DeviceHolder::new(device), mode);
        let connection = mounted.connect("main.db");
        connection
            .execute_batch(
                "CREATE TABLE t (k INTEGER PRIMARY KEY, v BLOB); INSERT INTO t VALUES (1, x'01')",
            )
            .unwrap();
        failing.store(true, Ordering::Relaxed);
        let failed = connection.execute("INSERT INTO t VALUES (2, zeroblob(100000))", []);
        assert_eq!(
            failed.unwrap_err().sqlite_error_code(),
            Some(rusqlite::ErrorCode::SystemIoFailure)
        );
        assert_eq!(
            mounted.file_set.check().unwrap_err().to_string(),
            "the file set failed: committing the file set's root: the device fails"
        );
        let device = mounted.snapshot();
        drop(connection);

        let reopened = Mounted::new(device, mode);
        reopened.check_extents();
        let connection = reopened.connect("main.db");
        assert_eq!(rows(&connection), BTreeMap::from([(1, vec![1])]));
    });
}

/// WAL mode would keep its log in memory, lost in a crash: SQLite keeps the rollback journal.
#[test]
fn wal_mode_is_refused() {
    both(|mode| {
        let mounted = Mounted::new(DeviceHolder::new(device()), mode);
        let connection = mounted.connect("main.db");
        connection.execute_batch("CREATE TABLE t (k INTEGER PRIMARY KEY)").unwrap();
        let mode: String =
            connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0)).unwrap();
        assert_eq!(mode, "delete");

        // In exclusive locking mode SQLite needs no shared memory for WAL mode, and takes it, but
        // fails to open the log at the next transaction.
        connection.execute_batch("PRAGMA locking_mode = EXCLUSIVE").unwrap();
        let mode: String =
            connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0)).unwrap();
        assert_eq!(mode, "wal");
        let inserted = connection.execute("INSERT INTO t VALUES (1)", []);
        assert_eq!(
            inserted.unwrap_err().sqlite_error_code(),
            Some(rusqlite::ErrorCode::CannotOpen)
        );
        assert_eq!(mounted.file_set.files().unwrap(), vec!["main.db".to_owned()]);
    });
}

/// Through a window, SQLite reads pages in place, and what a transaction changes reads back as it
/// left it, from the window, once committed, whether or not SQLite held pages as it committed.
#[test]
fn mapped_reads_follow_commits() {
    let mounted = Mounted::new(DeviceHolder::new(device()), Mode::Window);
    let writer = mounted.connect("main.db");
    let reader = mounted.connect("main.db");
    writer.execute_batch("CREATE TABLE t (k INTEGER PRIMARY KEY, v BLOB)").unwrap();
    for k in 0..300i64 {
        writer.execute("INSERT INTO t VALUES (?1, ?2)", params![k, vec![1u8; 500]]).unwrap();
    }
    let before = mounted.file_set.fetched();
    assert_eq!(rows(&reader).len(), 300);
    assert!(
        mounted.file_set.fetched() > before + 20,
        "{} pages fetched",
        mounted.file_set.fetched() - before
    );
    // A reader holding a statement open, its pages fetched, while the writer commits.
    let mut statement = reader.prepare("SELECT v FROM t ORDER BY k").unwrap();
    let mut held = statement.query([]).unwrap();
    let first: Vec<u8> = held.next().unwrap().unwrap().get(0).unwrap();
    assert_eq!(first, vec![1u8; 500]);
    let busy = writer.execute("UPDATE t SET v = ?1", params![vec![2u8; 500]]);
    assert_eq!(busy.unwrap_err().sqlite_error_code(), Some(rusqlite::ErrorCode::DatabaseBusy));
    drop(held);
    drop(statement);
    writer.execute("UPDATE t SET v = ?1", params![vec![2u8; 500]]).unwrap();
    let read = rows(&reader);
    assert_eq!(read.len(), 300);
    assert!(read.values().all(|v| *v == vec![2u8; 500]));
    writer.execute_batch("DELETE FROM t WHERE k >= 10; VACUUM").unwrap();
    assert_eq!(rows(&reader), (0..10).map(|k| (k, vec![2u8; 500])).collect());
    assert_eq!(integrity(&reader), "ok");
}

/// The window maps what a commit changed again only once no page is fetched: a page fetched
/// reads as it was meanwhile, and what the commit changed reads as committed, from the device.
#[test]
fn a_commit_waits_for_fetched_pages_to_remap() {
    let mounted = Mounted::new(DeviceHolder::new(device()), Mode::Window);
    let file_set = &mounted.file_set;
    file_set.open_file("f", true).unwrap();
    file_set.write("f", 0, &[1; 8192]).unwrap();
    file_set.sync("f").unwrap();
    let fetched = file_set.fetch("f", 4096, 4096).unwrap().expect("a page in place");
    // SAFETY: fetched, until given back.
    let page = || unsafe { std::slice::from_raw_parts(fetched, 4096) }.to_vec();
    assert_eq!(page(), vec![1; 4096]);

    file_set.write("f", 4096, &[2; 4096]).unwrap();
    assert_eq!(file_set.fetch("f", 4096, 4096).unwrap(), None, "changed since the last commit");
    file_set.sync("f").unwrap();
    assert_eq!(page(), vec![1; 4096], "held as fetched");
    assert_eq!(file_set.fetch("f", 4096, 4096).unwrap(), None, "the window is behind");
    let mut read = vec![0; 8192];
    assert_eq!(file_set.read("f", 0, &mut read).unwrap(), 8192);
    assert_eq!(read[4096..], [2; 4096]);

    file_set.unfetch("f");
    let fetched = file_set.fetch("f", 4096, 4096).unwrap().expect("mapped again");
    // SAFETY: fetched, until given back.
    assert_eq!(unsafe { std::slice::from_raw_parts(fetched, 4096) }, [2; 4096]);
    file_set.unfetch("f");
    file_set.close_file("f");
}

/// A commit leaving more extents unreferenced than a root names, as a VACUUM emptying a database
/// does, lists them in an extent of its own: a crash at each barrier leaves the database as of a
/// transaction, and the device holding what the file set does, those extents freed or to be.
#[test]
fn a_commit_freeing_many_extents_lists_them() {
    both(|mode| {
        let crashes = Crashes::new(7, 1);
        let mounted =
            Mounted::new(DeviceHolder::new(device().record_crashes(crashes.clone())), mode);
        let connection = mounted.connect("main.db");
        connection.execute_batch("CREATE TABLE t (k INTEGER PRIMARY KEY, v BLOB)").unwrap();
        for k in 0..400i64 {
            connection
                .execute("INSERT INTO t VALUES (?1, ?2)", params![k, vec![k as u8; 3000]])
                .unwrap();
        }
        let before = rows(&connection);
        let extents = mounted.file_set.extents().unwrap().len();
        assert!(extents > 64, "{extents} extents");
        // A crash at each transaction's barrier keeps what the transactions before it committed.
        crashes.mark(0);
        connection.execute_batch("DELETE FROM t").unwrap();
        crashes.mark(1);
        connection.execute_batch("VACUUM").unwrap();
        crashes.mark(2);
        connection.execute("INSERT INTO t VALUES (1, x'01')", []).unwrap();
        drop(connection);
        mounted.check_extents();

        let committed = [before, BTreeMap::new(), BTreeMap::new()];
        let mut marks = Vec::new();
        for (mark, device) in crashes.take() {
            let reopened = Mounted::new(DeviceHolder::new(device), mode);
            reopened.check_extents();
            let connection = reopened.connect("main.db");
            assert_eq!(integrity(&connection), "ok", "mark {mark}");
            assert_eq!(rows(&connection), committed[mark as usize], "mark {mark}");
            marks.push(mark);
        }
        // A barrier a transaction, but VACUUM's two: SQLite syncs the database twice in it.
        assert_eq!(marks, vec![0, 1, 1, 2]);
    });
}

/// A database closed and opened again after each round of transactions, rounds that free a few
/// extents or many (DELETE, VACUUM, rewriting every row), opens as its last transaction left it.
/// No test closed a file set after a commit whose dying extents its root listed in an extent of
/// their own, which closing freed and opening must read (TASK-540).
#[test]
fn a_database_opens_as_it_was_closed_after_every_kind_of_transaction() {
    both(|mode| {
        let mut rng = StdRng::seed_from_u64(1);
        let mut mounted = Mounted::new(DeviceHolder::new(device()), mode);
        let connection = mounted.connect("main.db");
        connection.execute_batch("CREATE TABLE t (k INTEGER PRIMARY KEY, v BLOB)").unwrap();
        drop(connection);
        for round in 0..12 {
            let connection = mounted.connect("main.db");
            // Transactions of a few rows each, an extent or so each, then one that may free them.
            for _ in 0..rng.random_range(100..250) {
                let transaction = connection.unchecked_transaction().unwrap();
                for _ in 0..rng.random_range(1..8) {
                    let (k, len) = (rng.random_range(0..2000i64), rng.random_range(1..4000usize));
                    transaction
                        .execute(
                            "INSERT OR REPLACE INTO t VALUES (?1, ?2)",
                            params![k, vec![round as u8; len]],
                        )
                        .unwrap();
                }
                transaction.commit().unwrap();
            }
            match rng.random_range(0..3) {
                0 => connection.execute_batch("DELETE FROM t").unwrap(),
                1 => connection.execute_batch("VACUUM").unwrap(),
                _ => connection
                    .execute("UPDATE t SET v = randomblob(length(v))", [])
                    .map(drop)
                    .unwrap(),
            }
            let expected = rows(&connection);
            drop(connection);
            mounted.check_extents();

            mounted = Mounted::new(mounted.close(), mode);
            mounted.check_extents();
            let connection = mounted.connect("main.db");
            assert_eq!(integrity(&connection), "ok", "{mode:?}, round {round}");
            assert_eq!(rows(&connection), expected, "{mode:?}, round {round}");
        }
    });
}

/// A connection SQLite opened on the VFS with none of its files open, an in-memory database's,
/// outlives the registration: SQLite keeps the VFS and calls it after, for the time among others.
/// The VFS freed with the registration, which looked only for files open, SQLite called through
/// freed memory (TASK-444.14; AddressSanitizer shows it, as Miri runs no C).
#[test]
fn a_connection_outlives_its_registration() {
    let file_set = FileSet::open(DeviceHolder::new(device()), Options::default()).unwrap();
    let registration = register("cladefs-test-outlived", file_set).unwrap();
    let connection = Connection::open_with_flags_and_vfs(
        ":memory:",
        OpenFlags::default(),
        "cladefs-test-outlived",
    )
    .unwrap();
    drop(registration);
    let now: f64 = connection.query_row("SELECT julianday('now')", [], |row| row.get(0)).unwrap();
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
    // The Julian day of the Unix epoch, and a day's seconds.
    let expected = 2_440_587.5 + since.as_secs_f64() / 86_400.0;
    assert!((now - expected).abs() < 10.0 / 86_400.0, "{now} is not {expected}, within 10 s");
}

/// TASK-530: closing a file that was never committed returns its region for the next open.
#[test]
fn closing_an_uncommitted_file_returns_its_mapping_region() {
    let files = FileSet::open_mapped(
        DeviceHolder::new(device()),
        Box::new(CopyingWindow::new(WINDOW, BLOCK)),
        Options { window_regions: 1, ..Options::default() },
    )
    .unwrap();
    files.open_file("empty", true).unwrap();
    files.close_file("empty");
    assert!(!files.exists("empty").unwrap());

    files.open_file("database", true).unwrap();
    files.write("database", 0, &[5; BLOCK as usize]).unwrap();
    files.sync("database").unwrap();
    let mapped = files.fetch("database", 0, BLOCK as u64).unwrap();
    assert!(mapped.is_some(), "the only mapping region should be available again");
    files.unfetch("database");
}
