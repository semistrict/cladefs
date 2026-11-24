// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Tests of images and their volumes: filesystems of their own on one image.

use cladefs::{Errno, Filesystem, Image, Owner, RenameFlags, Storage, VolumeMetadata};
use futures::executor::block_on;

const OWNER: Owner = Owner { uid: 1000, gid: 100 };

/// The size of the images the tests format.
const STORAGE_SIZE: u64 = 32 << 20;

async fn format() -> Image {
    Image::format(Storage::memory(STORAGE_SIZE)).await.expect("format failed")
}

/// Creates `name` in the root of `fs`, holding `contents`.
async fn write_file(fs: &Filesystem, name: &[u8], contents: &[u8]) {
    let (file, _) = fs.root().create(name, 0o644, OWNER).await.unwrap();
    assert_eq!(file.write(0, contents).await, Ok(contents.len()));
}

/// The contents of `name` in the root of `fs`.
async fn read_file(fs: &Filesystem, name: &[u8]) -> Vec<u8> {
    let (file, attr) = fs.root().lookup(name).await.unwrap();
    let mut buf = vec![0; attr.size as usize];
    assert_eq!(file.read(0, &mut buf).await, Ok(buf.len()));
    buf
}

/// A new image has no volumes; created ones are listed, by name.
#[test]
fn create_and_list_volumes() {
    block_on(async {
        let image = format().await;
        assert_eq!(image.volumes().await.unwrap(), Vec::<String>::new());
        let b = image.create_volume("b").await.unwrap();
        let a = image.create_volume("a").await.unwrap();
        assert_eq!(image.volumes().await.unwrap(), ["a", "b"]);
        let exists = image.create_volume("a").await.expect_err("created a volume twice");
        assert_eq!(exists.errno(), Some(Errno::EEXIST));
        let missing = image.open_volume("c").await.expect_err("opened a missing volume");
        assert_eq!(missing.errno(), Some(Errno::ENOENT));
        let mounted = image.open_volume("a").await.expect_err("mounted a volume twice");
        assert_eq!(mounted.errno(), Some(Errno::EBUSY));
        a.unmount().await.unwrap();
        b.unmount().await.unwrap();
        image.close().await.unwrap().check().await.unwrap();
    })
}

/// A volume's metadata is its own: layers once the journal is compacted, and a record of the
/// allocator's for each extent.
#[test]
fn volume_metadata() {
    block_on(async {
        let image = format().await;
        let a = image.create_volume("a").await.unwrap();
        let b = image.create_volume("b").await.unwrap();
        assert_eq!(
            a.metadata().await.unwrap(),
            VolumeMetadata { layer_bytes: 0, allocator_records: 0 }
        );

        write_file(&a, b"file", &[1; 1 << 20]).await;
        let (file, _) = a.root().lookup(b"file").await.unwrap();
        file.fsync().await.unwrap();
        image.compact().await.unwrap();

        let written = a.metadata().await.unwrap();
        assert_eq!(written.allocator_records, 1);
        assert!(written.layer_bytes > 0, "{written:?}");
        assert_eq!(b.metadata().await.unwrap().allocator_records, 0);

        drop(file);
        a.unmount().await.unwrap();
        b.unmount().await.unwrap();
        image.close().await.unwrap().check().await.unwrap();
    })
}

/// What one volume holds, another doesn't.
#[test]
fn volumes_are_isolated() {
    block_on(async {
        let image = format().await;
        let a = image.create_volume("a").await.unwrap();
        let b = image.create_volume("b").await.unwrap();
        write_file(&a, b"file", b"in a").await;
        assert_eq!(b.root().lookup(b"file").await.map(|_| ()), Err(Errno::ENOENT));
        write_file(&b, b"file", b"in b").await;
        assert_eq!(read_file(&a, b"file").await, b"in a");
        assert_eq!(read_file(&b, b"file").await, b"in b");
        b.root().unlink(b"file").await.unwrap();
        assert_eq!(read_file(&a, b"file").await, b"in a");
        image.close().await.unwrap().check().await.unwrap();
    })
}

/// Unmounting one volume leaves the others served; its filesystem then fails with `ENOTCONN`,
/// and it can be mounted again, with what was written to it.
#[test]
fn unmounting_a_volume_leaves_the_others() {
    block_on(async {
        let image = format().await;
        let a = image.create_volume("a").await.unwrap();
        let b = image.create_volume("b").await.unwrap();
        write_file(&a, b"file", b"kept").await;
        a.unmount().await.unwrap();
        assert_eq!(a.root().getattr().await.map(|_| ()), Err(Errno::ENOTCONN));
        write_file(&b, b"file", b"still served").await;
        // A volume's storage is its image's.
        assert!(a.close().await.is_err());
        let a = image.open_volume("a").await.unwrap();
        assert_eq!(read_file(&a, b"file").await, b"kept");
        drop(a);
        image.close().await.unwrap().check().await.unwrap();
    })
}

/// Dropping a volume's filesystem unmounts it, writing back what it held: it can be mounted again.
#[test]
fn dropping_a_volume_unmounts_it() {
    block_on(async {
        let image = format().await;
        let a = image.create_volume("a").await.unwrap();
        write_file(&a, b"file", b"dropped").await;
        drop(a);
        let a = image.open_volume("a").await.unwrap();
        assert_eq!(read_file(&a, b"file").await, b"dropped");
        drop(a);
        image.close().await.unwrap().check().await.unwrap();
    })
}

/// Closing an image unmounts the volumes still mounted, and what they held survives.
#[test]
fn closing_the_image_unmounts_its_volumes() {
    block_on(async {
        let image = format().await;
        let a = image.create_volume("a").await.unwrap();
        write_file(&a, b"file", b"written before closing").await;
        let storage = image.close().await.unwrap();
        assert_eq!(a.root().getattr().await.map(|_| ()), Err(Errno::ENOTCONN));
        drop(a);

        let image = Image::mount(storage.check().await.unwrap()).await.unwrap();
        assert_eq!(image.volumes().await.unwrap(), ["a"]);
        let a = image.open_volume("a").await.unwrap();
        assert_eq!(read_file(&a, b"file").await, b"written before closing");
        drop(a);
        image.close().await.unwrap();
    })
}

/// A filesystem mounted from storage is the image's `default` volume, which an image sees.
#[test]
fn a_filesystem_is_the_default_volume() {
    block_on(async {
        let fs = Filesystem::format(Storage::memory(STORAGE_SIZE)).await.unwrap();
        write_file(&fs, b"file", b"single").await;
        let image = Image::mount(fs.close().await.unwrap()).await.unwrap();
        assert_eq!(image.volumes().await.unwrap(), ["default"]);
        let fs = image.open_volume("default").await.unwrap();
        assert_eq!(read_file(&fs, b"file").await, b"single");
        drop(fs);
        let fs = Filesystem::mount(image.close().await.unwrap()).await.unwrap();
        assert_eq!(read_file(&fs, b"file").await, b"single");
        fs.close().await.unwrap().check().await.unwrap();
    })
}

/// A node is its own filesystem's: renames and links into another volume's directories fail with
/// `EXDEV`, and change nothing.
#[test]
fn nodes_of_two_volumes_do_not_mix() {
    block_on(async {
        let image = format().await;
        let a = image.create_volume("a").await.unwrap();
        let b = image.create_volume("b").await.unwrap();
        let (file, _) = a.root().create(b"file", 0o644, OWNER).await.unwrap();
        let renamed = a.root().rename(b"file", b.root(), b"moved", RenameFlags::empty()).await;
        assert_eq!(renamed, Err(Errno::EXDEV));
        assert_eq!(file.link(b.root(), b"link").await.map(|_| ()), Err(Errno::EXDEV));
        assert_eq!(b.root().entries().await.unwrap(), []);
        assert_eq!(a.root().lookup(b"file").await.unwrap().1.inode, file.inode());
        image.close().await.unwrap().check().await.unwrap();
    })
}
