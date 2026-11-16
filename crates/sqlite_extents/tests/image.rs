// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! SQLite on a volume on an cladefs image: an extent store on disk, its branch kept in a bucket
//! beside it (TASK-296).

mod common;

use cladefs::Storage;
use rusqlite::{Connection, OpenFlags, params};
use sqlite_extents::{Options, Registration, Volume, register};
use std::path::Path;
use std::sync::Arc;

#[test]
fn a_database_on_an_image_survives_closing() {
    let found = survives_closing("plain");
    assert!(!found.is_empty(), "no file of the image holds its data in the clear");
}

/// TASK-411.7 (TASK-411's AC #3): a database on an image of a sealed cell, its tenant's KEK in the
/// host's KMS, survives closing as one in the clear does; and no file of the image, its store's
/// nor its bucket's, holds its data in the clear.
#[test]
fn a_sealed_database_on_an_image_survives_closing() {
    let keys = tempfile::tempdir().unwrap();
    let kms = extent_keys::kms::LocalKms::open(keys.path().join("kms")).unwrap();
    // The tenant of the images `Storage` makes: the common tenant's, 0.
    kms.create(0).unwrap();
    cladefs::bucket::set_kms(Arc::new(kms)).unwrap();
    assert_eq!(survives_closing("sealed"), Vec::<String>::new());
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
        let volume = Volume::open(
            Storage::create(&path, 256 << 20).unwrap().into_device(),
            Options::default(),
        )
        .unwrap();
        let registration = register(&write, volume.clone()).unwrap();
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
        volume.close().unwrap();
    }

    let volume =
        Volume::open(Storage::open(&path).unwrap().into_device(), Options::default()).unwrap();
    assert_eq!(volume.files().unwrap(), vec!["app.db".to_owned()]);
    let registration = register(&read, volume.clone()).unwrap();
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
    volume.close().unwrap();

    cladefs::testing::files_holding(dir.path(), &[&marker[..64]])
}

/// A volume on `storage`, registered as the VFS `vfs`, and a connection to its database.
fn mount(storage: Storage, vfs: &str) -> (Arc<Volume>, Registration, Connection) {
    let volume = Volume::open(storage.into_device(), Options::default()).unwrap();
    let registration = register(vfs, volume.clone()).unwrap();
    let connection =
        Connection::open_with_flags_and_vfs("app.db", OpenFlags::default(), vfs).unwrap();
    (volume, registration, connection)
}

fn unmount((volume, registration, connection): (Arc<Volume>, Registration, Connection)) {
    drop(connection);
    held(&volume);
    drop(registration);
    volume.close().unwrap();
}

/// Checks the device holds every extent the volume names, those it references and those it has yet
/// to free: so that none it frees, or failed to, is one the device never held, which it would try
/// to free again at each commit.
fn held(volume: &Volume) {
    let device = volume.device();
    let mut buffer =
        futures::executor::block_on(device.allocate_buffer(device.block_size() as usize));
    for extent in volume.extents().unwrap() {
        futures::executor::block_on(device.read(extent, 0, buffer.as_mut()))
            .unwrap_or_else(|error| panic!("{extent:?} the volume names: {error:#}"));
    }
}

fn open(path: &Path) -> Storage {
    Storage::open(path).unwrap()
}

/// A clone of an image, a fork of its branch in its bucket, holds the image's database as it was,
/// inheriting its extents; the two then change apart, the clone rewriting and so freeing all it
/// inherited, and each reads its own, before and after opening again.
#[test]
fn a_clone_and_its_image_go_their_own_ways() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (path, clone) = (dir.path().join("image"), dir.path().join("clone"));
    let mounted = mount(Storage::create(&path, 256 << 20).unwrap(), "cladefs-clone-fill");
    let mut parent_rows = common::fill(&mounted.2);
    unmount(mounted);

    let cloned = futures::executor::block_on(open(&path).clone_to(&clone)).unwrap();
    let parent = mount(open(&path), "cladefs-clone-parent");
    let fork = mount(cloned, "cladefs-clone-fork");
    let mut fork_rows = parent_rows.clone();
    common::check(&fork.2, &fork_rows);
    common::change_parent(&parent.2, &mut parent_rows);
    common::change_fork(&fork.2, &mut fork_rows);
    common::check(&parent.2, &parent_rows);
    common::check(&fork.2, &fork_rows);
    unmount(fork);
    unmount(parent);

    for (path, rows) in [(&path, parent_rows), (&clone, fork_rows)] {
        let mounted = mount(open(path), "cladefs-clone-reopened");
        common::check(&mounted.2, &rows);
        unmount(mounted);
    }
}
