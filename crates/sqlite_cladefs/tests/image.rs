// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! SQLite on a cladefs image: an extent store on disk, its cell kept in a bucket
//! beside it (TASK-296).

use cladefs::Storage;
use rusqlite::{Connection, OpenFlags, params};
use sqlite_cladefs::{Database, Options, register};
use std::sync::Arc;

/// A database on an image survives closing; and, every cell encrypted (TASK-411.28), under the
/// tests' KMS here, no file of the image, its cell's nor its bucket's, holds its data in the clear.
#[tokio::test(flavor = "multi_thread")]
async fn a_database_on_an_image_survives_closing() {
    assert_eq!(survives_closing("plain").await, Vec::<String>::new());
}

/// TASK-411.7 (TASK-411's AC #3): a database on an image of a cell whose tenant's KEK is in the
/// KMS the host names survives closing, and no file of the image, its cell's nor its bucket's,
/// holds its data in the clear.
#[tokio::test(flavor = "multi_thread")]
async fn an_encrypted_database_on_an_image_survives_closing() {
    let keys = tempfile::tempdir().unwrap();
    let kms = extent_keys::kms::LocalKms::open(keys.path().join("kms")).unwrap();
    // The tenant of the images `Storage` makes: the default tenant's, 0.
    kms.create(0).unwrap();
    cladefs::bucket::set_kms(Arc::new(kms)).unwrap();
    assert_eq!(survives_closing("encrypted").await, Vec::<String>::new());
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
async fn survives_closing(name: &str) -> Vec<String> {
    // Under the build's temporary directory, where direct I/O works, as cladefs's image tests have.
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = dir.path().join("image");
    let marker = noise(4096);
    let (write, read) =
        (format!("cladefs-image-write-{name}"), format!("cladefs-image-read-{name}"));
    {
        let database = Database::open(
            Storage::create(&path, 256 << 20).await.unwrap().into_device(),
            Options::default(),
        )
        .unwrap();
        let registration = register(&write, database.clone()).unwrap();
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
        database.close().unwrap();
    }

    let database =
        Database::open(Storage::open(&path).await.unwrap().into_device(), Options::default())
            .unwrap();
    let registration = register(&read, database.clone()).unwrap();
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
    database.close().unwrap();

    cladefs::testing::files_holding(dir.path(), &[&marker[..64]])
}
