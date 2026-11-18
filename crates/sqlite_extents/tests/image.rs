// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! SQLite on a file set on an cladefs image: an extent store on disk, its cell kept in a bucket
//! beside it (TASK-296).

mod common;

use cladefs::Storage;
use rusqlite::{Connection, OpenFlags, params};
use sqlite_extents::{FileSet, Options, Registration, register};
use std::path::Path;
use std::sync::Arc;

/// A database on an image survives closing; and, every store encrypted (TASK-411.28), under the
/// tests' KMS here, no file of the image, its store's nor its bucket's, holds its data in the clear.
#[test]
fn a_database_on_an_image_survives_closing() {
    assert_eq!(survives_closing("plain"), Vec::<String>::new());
}

/// TASK-411.7 (TASK-411's AC #3): a database on an image of a cell whose tenant's KEK is in the
/// KMS the host names survives closing, and no file of the image, its store's nor its bucket's,
/// holds its data in the clear.
#[test]
fn an_encrypted_database_on_an_image_survives_closing() {
    let keys = tempfile::tempdir().unwrap();
    let kms = extent_keys::kms::LocalKms::open(keys.path().join("kms")).unwrap();
    // The tenant of the images `Storage` makes: the default tenant's, 0.
    kms.create(0).unwrap();
    cladefs::bucket::set_kms(Arc::new(kms)).unwrap();
    assert_eq!(survives_closing("encrypted"), Vec::<String>::new());
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

/// Writes a database on a new image, closes it, opens it again and checks it, its VFSs named after
/// `name`: returns the files of the image holding the data of a row of [`noise`] in the clear.
fn survives_closing(name: &str) -> Vec<String> {
    // Under the build's temporary directory, where direct I/O works, as cladefs's image tests have.
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("image");
    let marker = noise(4096);
    let (write, read) =
        (format!("cladefs-image-write-{name}"), format!("cladefs-image-read-{name}"));
    {
        let file_set = FileSet::open(
            Storage::create(&path, 256 << 20).unwrap().into_device(),
            Options::default(),
        )
        .unwrap();
        let registration = register(&write, file_set.clone()).unwrap();
        let connection =
            Connection::open_with_flags_and_vfs("app.db", OpenFlags::default(), write.as_str())
                .unwrap();
        connection.execute_batch("CREATE TABLE t (k INTEGER PRIMARY KEY, v BLOB)").unwrap();
        for round in 0..10i64 {
            let transaction = connection.unchecked_transaction().unwrap();
            for k in 0..300 {
                transaction
                    .execute(
                        "INSERT OR REPLACE INTO t VALUES (?1, ?2)",
                        params![k, vec![round as u8; 700]],
                    )
                    .unwrap();
            }
            transaction.commit().unwrap();
        }
        connection.execute("INSERT INTO t VALUES (?1, ?2)", params![1000, marker]).unwrap();
        drop(connection);
        drop(registration);
        file_set.close().unwrap();
    }

    let file_set =
        FileSet::open(Storage::open(&path).unwrap().into_device(), Options::default()).unwrap();
    assert_eq!(file_set.files().unwrap(), vec!["app.db".to_owned()]);
    let registration = register(&read, file_set.clone()).unwrap();
    let connection =
        Connection::open_with_flags_and_vfs("app.db", OpenFlags::default(), read.as_str()).unwrap();
    let check: String =
        connection.query_row("PRAGMA integrity_check", [], |row| row.get(0)).unwrap();
    assert_eq!(check, "ok");
    let (count, sum): (i64, i64) = connection
        .query_row(
            "SELECT count(*), sum(unicode(substr(v, 1, 1))) FROM t WHERE k < 1000",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((count, sum), (300, 300 * 9));
    let read_marker: Vec<u8> =
        connection.query_row("SELECT v FROM t WHERE k = 1000", [], |row| row.get(0)).unwrap();
    assert!(read_marker == marker, "the row of noise reads back otherwise");
    drop(connection);
    drop(registration);
    file_set.close().unwrap();

    cladefs::testing::files_holding(dir.path(), &[&marker[..64]])
}

/// A file set on `storage`, registered as the VFS `vfs`, and a connection to its database.
fn mount(storage: Storage, vfs: &str) -> (Arc<FileSet>, Registration, Connection) {
    let file_set = FileSet::open(storage.into_device(), Options::default()).unwrap();
    let registration = register(vfs, file_set.clone()).unwrap();
    let connection =
        Connection::open_with_flags_and_vfs("app.db", OpenFlags::default(), vfs).unwrap();
    (file_set, registration, connection)
}

fn unmount((file_set, registration, connection): (Arc<FileSet>, Registration, Connection)) {
    drop(connection);
    held(&file_set);
    drop(registration);
    file_set.close().unwrap();
}

/// Checks the device holds every extent the file set names, those it references and those it has
/// yet to free: so that none it frees, or failed to, is one the device never held, which it would
/// try to free again at each commit.
fn held(file_set: &FileSet) {
    let device = file_set.device();
    let mut buffer =
        futures::executor::block_on(device.allocate_buffer(device.block_size() as usize));
    for extent in file_set.extents().unwrap() {
        futures::executor::block_on(device.read(extent, 0, buffer.as_mut()))
            .unwrap_or_else(|error| panic!("{extent:?} the file set names: {error:#}"));
    }
}

fn open(path: &Path) -> Storage {
    Storage::open(path).unwrap()
}

/// A fork of an image, its cell forked in its bucket, holds the image's database as it was,
/// inheriting its extents; the two then change apart, the fork rewriting and so freeing all it
/// inherited, and each reads its own, before and after opening again.
#[test]
fn a_fork_and_its_image_go_their_own_ways() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (path, fork_path) = (dir.path().join("image"), dir.path().join("fork"));
    let mounted = mount(Storage::create(&path, 256 << 20).unwrap(), "cladefs-fork-fill");
    let mut parent_rows = common::fill(&mounted.2);
    unmount(mounted);

    let forked = futures::executor::block_on(open(&path).fork_to(&fork_path)).unwrap();
    let parent = mount(open(&path), "cladefs-fork-parent");
    let fork = mount(forked, "cladefs-fork-child");
    let mut fork_rows = parent_rows.clone();
    common::check(&fork.2, &fork_rows);
    common::change_parent(&parent.2, &mut parent_rows);
    common::change_fork(&fork.2, &mut fork_rows);
    common::check(&parent.2, &parent_rows);
    common::check(&fork.2, &fork_rows);
    unmount(fork);
    unmount(parent);

    for (path, rows) in [(&path, parent_rows), (&fork_path, fork_rows)] {
        let mounted = mount(open(path), "cladefs-fork-reopened");
        common::check(&mounted.2, &rows);
        unmount(mounted);
    }
}
