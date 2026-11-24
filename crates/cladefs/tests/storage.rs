// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Tests of image storage: direct I/O and deletion.
//!
//! Images live under the build's temporary directory, which is on the repository's filesystem,
//! where direct I/O works, rather than on a `/tmp` that may be a tmpfs.

use cladefs::{Filesystem, Owner, Storage};
use futures::executor::block_on;
use std::path::Path;

const OWNER: Owner = Owner { uid: 1000, gid: 100 };
const IMAGE_SIZE: u64 = 64 << 20;

/// A new directory for a test's images.
fn images() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

/// A formatted image at `path`, holding `file` with `contents`, closed.
async fn image_with(path: &Path, file: &[u8], contents: &[u8]) -> Storage {
    let fs = Filesystem::format(Storage::create(path, IMAGE_SIZE).await.unwrap()).await.unwrap();
    let (node, _) = fs.root().create(file, 0o644, OWNER).await.unwrap();
    node.write(0, contents).await.unwrap();
    fs.close().await.unwrap()
}

/// Image I/O bypasses the host's page cache.
#[tokio::test(flavor = "multi_thread")]
async fn images_bypass_the_page_cache() {
    let dir = images();
    let storage = Storage::create(dir.path().join("image"), IMAGE_SIZE).await.unwrap();
    assert!(storage.bypasses_page_cache());
    assert!(!Storage::memory(IMAGE_SIZE).bypasses_page_cache());
}

/// An image cannot be deleted while open; once closed, deleting it removes its directory and it
/// cannot be opened again.
#[test]
fn delete_image() {
    block_on(async {
        let dir = images();
        let path = dir.path().join("image");
        let storage = image_with(&path, b"file", b"original").await;
        assert!(Storage::delete(&path).await.is_err(), "deleted an open image");
        drop(storage);
        Storage::delete(&path).await.unwrap();
        assert!(!path.exists());
        assert!(Storage::open(&path).await.is_err(), "opened a deleted image");
    })
}
