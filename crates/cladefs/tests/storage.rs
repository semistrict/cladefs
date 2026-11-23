// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Tests of image storage: direct I/O, and forks of closed and mounted filesystems.
//!
//! Images live under the build's temporary directory, which is on the repository's filesystem,
//! where direct I/O works, rather than on a `/tmp` that may be a tmpfs.

use cladefs::{Filesystem, Owner, Storage};
use futures::executor::block_on;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

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

/// The contents of `file` in the root of `fs`.
async fn read(fs: &Filesystem, file: &[u8]) -> Vec<u8> {
    let (node, attr) = fs.root().lookup(file).await.unwrap();
    let mut buf = vec![0; attr.size as usize];
    assert_eq!(node.read(0, &mut buf).await, Ok(buf.len()));
    buf
}

/// The bytes of the host's storage the files under `path` take.
fn allocated(path: &Path) -> u64 {
    files(path).iter().map(|(_, bytes)| bytes).sum()
}

/// The files under `path`, each with the bytes of the host's storage it takes.
fn files(path: &Path) -> Vec<(PathBuf, u64)> {
    let metadata = std::fs::symlink_metadata(path).unwrap();
    if !metadata.is_dir() {
        return vec![(path.to_owned(), metadata.blocks() * 512)];
    }
    std::fs::read_dir(path).unwrap().flat_map(|entry| files(&entry.unwrap().path())).collect()
}

/// Image I/O bypasses the host's page cache.
#[tokio::test(flavor = "multi_thread")]
async fn images_bypass_the_page_cache() {
    let dir = images();
    let storage = Storage::create(dir.path().join("image"), IMAGE_SIZE).await.unwrap();
    assert!(storage.bypasses_page_cache());
    assert!(!Storage::memory(IMAGE_SIZE).bypasses_page_cache());
}

/// A fork of closed storage is a separate image with the same contents, which shares the image's
/// data rather than copies it.
#[test]
fn fork_closed_storage() {
    block_on(async {
        let dir = images();
        let (path, fork_path): (PathBuf, PathBuf) =
            (dir.path().join("image"), dir.path().join("fork"));
        let storage = image_with(&path, b"file", b"original").await;
        let before = allocated(&path);
        let fork = storage.fork_to(&fork_path).await.expect("fork failed");
        // The fork's cell directory holds its cell's records, of a 4 KiB block each or less: its
        // description, ancestry, open log and open segment, and the write-ahead log's start, two
        // blocks, a slot each. What the image holds stays in the image's cell, which the fork's
        // inherits (board 9392).
        assert!(allocated(&fork_path) <= 6 * (4 << 10), "{:?}", files(&fork_path));
        assert!(allocated(&path) <= before + (64 << 10), "{} > {before}", allocated(&path));
        assert!(storage.fork_to(&fork_path).await.is_err(), "forked over an image");

        // Each changes independently of the other.
        let fs = Filesystem::mount(fork.check().await.unwrap()).await.unwrap();
        assert_eq!(read(&fs, b"file").await, b"original");
        let (file, _) = fs.root().lookup(b"file").await.unwrap();
        file.write(0, b"modified").await.unwrap();
        fs.close().await.unwrap().check().await.unwrap();

        let fs = Filesystem::mount(storage.check().await.unwrap()).await.unwrap();
        assert_eq!(read(&fs, b"file").await, b"original");
        fs.close().await.unwrap();
    })
}

/// A fork of a mounted filesystem has what was written before it, even if never synced, and not
/// what is written after.
#[test]
fn fork_mounted_filesystem() {
    block_on(async {
        let dir = images();
        let (path, fork_path) = (dir.path().join("image"), dir.path().join("fork"));
        let storage = image_with(&path, b"before", b"synced").await;
        let fs = Filesystem::mount(storage).await.unwrap();
        let (file, _) = fs.root().create(b"unsynced", 0o644, OWNER).await.unwrap();
        file.write(0, b"dirty pages").await.unwrap();

        let fork = fs.fork_storage_to(&fork_path).await.expect("fork failed");
        file.write(0, b"after fork").await.unwrap();
        fs.close().await.unwrap().check().await.unwrap();

        let fs = Filesystem::mount(fork.check().await.unwrap()).await.unwrap();
        assert_eq!(read(&fs, b"before").await, b"synced");
        assert_eq!(read(&fs, b"unsynced").await, b"dirty pages");
        fs.close().await.unwrap();
    })
}

/// An image deleted while a fork of it lives goes, its directory with it: the fork mounts as
/// before, reading what it inherited in the image's bucket, which is beside the image's directory,
/// not in it (TASK-39.4, TASK-165.7). Before, the image's directory stayed, holding what the fork
/// shared of it, and went with the fork.
#[test]
fn delete_images_and_their_forks() {
    block_on(async {
        let dir = images();
        let (path, fork_path) = (dir.path().join("image"), dir.path().join("fork"));
        let storage = image_with(&path, b"file", b"original").await;
        let fork = storage.fork_to(&fork_path).await.unwrap();
        assert!(Storage::delete(&fork_path).await.is_err(), "deleted an open fork");
        assert!(Storage::delete(&path).await.is_err(), "deleted an open image");
        drop((fork, storage));
        Storage::delete(&path).await.unwrap();
        assert!(!path.exists());
        assert!(Storage::open(&path).await.is_err(), "opened a deleted image");
        let fs = Filesystem::mount(Storage::open(&fork_path).await.unwrap()).await.unwrap();
        assert_eq!(read(&fs, b"file").await, b"original");
        fs.close().await.unwrap();
        Storage::delete(&fork_path).await.unwrap();
        assert!(!fork_path.exists());
    })
}

#[test]
fn memory_cannot_be_forked() {
    block_on(async {
        let dir = images();
        let storage = Storage::memory(IMAGE_SIZE);
        let error = storage.fork_to(dir.path().join("fork")).await.expect_err("forked memory");
        assert_eq!(error.to_string(), "memory cannot be forked to an image");
        let fs = Filesystem::format(storage).await.unwrap();
        let error = fs.fork_storage_to(dir.path().join("fork")).await.expect_err("forked memory");
        assert_eq!(error.to_string(), "memory cannot be forked to an image");
        fs.close().await.unwrap();
    })
}
