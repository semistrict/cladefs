// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Tests of importing container images into filesystems (TASK-39.5).

#[cfg(target_os = "linux")]
mod common;

use cladefs::import::testing::{Entry, Kind, MTIME, gzip, layer, oci_layout, opaque, whiteout};
use cladefs::import::{Importer, OciImage};
use cladefs::{Attr, Errno, FileType, Filesystem, Node, Storage};
use futures::executor::block_on;
use std::future::poll_fn;
use std::os::unix::ffi::OsStringExt as _;
use std::path::Path;
use std::pin::pin;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The size of the filesystems the tests format.
const STORAGE_SIZE: u64 = 64 << 20;

fn time(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

/// A new filesystem, with the layers `layers` applied in turn.
async fn imported(layers: &[Vec<u8>]) -> Filesystem {
    let fs = Filesystem::format(Storage::memory(STORAGE_SIZE)).await.unwrap();
    let mut importer = Importer::new(&fs);
    for layer in layers {
        importer.apply(layer.as_slice()).await.expect("applying a layer failed");
    }
    importer.finish().await.expect("finishing failed");
    fs
}

/// Closes and checks `fs`.
async fn close(fs: Filesystem) {
    fs.close().await.unwrap().check().await.expect("check failed");
}

/// The node at `path`, and its attributes.
async fn at(fs: &Filesystem, path: &str) -> cladefs::Result<(Node, Attr)> {
    let mut node = fs.root().clone();
    let mut attr = node.getattr().await?;
    for name in path.split('/').filter(|name| !name.is_empty()) {
        (node, attr) = node.lookup(name.as_bytes()).await?;
    }
    Ok((node, attr))
}

/// The attributes of the node at `path`.
async fn attr_at(fs: &Filesystem, path: &str) -> cladefs::Result<Attr> {
    Ok(at(fs, path).await?.1)
}

async fn contents(file: &Node) -> Vec<u8> {
    let mut buf = vec![0; file.getattr().await.unwrap().size as usize];
    let mut read = 0;
    while read < buf.len() {
        let len = file.read(read as u64, &mut buf[read..]).await.unwrap();
        assert_ne!(len, 0, "the file ends early");
        read += len;
    }
    buf
}

/// The names in the directory at `path`, sorted.
async fn names(fs: &Filesystem, path: &str) -> Vec<String> {
    let (dir, _) = at(fs, path).await.unwrap();
    let entries = dir.entries().await.unwrap();
    let mut names: Vec<String> =
        entries.into_iter().map(|e| String::from_utf8(e.name).unwrap()).collect();
    names.sort();
    names
}

#[test]
fn a_layer_makes_files_with_their_owners_modes_links_and_xattrs() {
    let holey = [&b"head"[..], &[0; 128 << 10], b"tail", &[0; 128 << 10]].concat();
    let capability = [1, 0, 0, 2, 0x20, 0, 0, 0];
    let layer = layer(&[
        Entry::new("etc", Kind::Dir).mtime(MTIME + 1),
        Entry::new("etc/passwd", Kind::File(b"root:x:0:0::/root:/bin/sh\n")).mtime(MTIME + 2),
        Entry::new("tmp", Kind::Dir).perm(0o1777),
        Entry::new("usr/bin/sudo", Kind::File(b"#!/bin/sudo")).perm(0o4755),
        Entry::new("bin/ping", Kind::File(b"ping"))
            .perm(0o755)
            .xattrs(&[("security.capability", &capability), ("user.origin", b"test")]),
        Entry::new("home/user", Kind::Dir).perm(0o700).owner(1000, 1000),
        Entry::new("home/user/notes", Kind::File(b"mine")).perm(0o600).owner(1000, 100),
        Entry::new("home/user/link", Kind::Symlink("notes")).owner(1000, 100),
        Entry::new("home/user/hard", Kind::HardLink("home/user/notes")),
        Entry::new("dev/null", Kind::CharDevice(1, 3)).perm(0o666),
        Entry::new("dev/sda1", Kind::BlockDevice(8, 1)).perm(0o660).owner(0, 6),
        Entry::new("run/fifo", Kind::Fifo).perm(0o620),
        Entry::new("var/holey", Kind::File(&holey)),
        Entry::new("var/zeros", Kind::File(&[0; 128 << 10])),
    ])
    .unwrap();
    block_on(async {
        let fs = imported(&[layer]).await;
        let etc = attr_at(&fs, "etc").await.unwrap();
        assert_eq!(
            (etc.file_type, etc.perm, etc.mtime),
            (FileType::Directory, 0o755, time(MTIME + 1))
        );
        let (passwd_file, passwd) = at(&fs, "etc/passwd").await.unwrap();
        assert_eq!((passwd.perm, passwd.uid, passwd.gid), (0o644, 0, 0));
        assert_eq!((passwd.mtime, passwd.atime), (time(MTIME + 2), time(MTIME + 2)));
        assert_eq!(contents(&passwd_file).await, b"root:x:0:0::/root:/bin/sh\n");
        assert_eq!(attr_at(&fs, "tmp").await.unwrap().perm, 0o1777);
        assert_eq!(attr_at(&fs, "usr/bin/sudo").await.unwrap().perm, 0o4755);
        // Directories no entry made: root's, 0755, at the epoch.
        let usr = attr_at(&fs, "usr").await.unwrap();
        assert_eq!((usr.perm, usr.uid, usr.mtime), (0o755, 0, UNIX_EPOCH));

        let (ping, _) = at(&fs, "bin/ping").await.unwrap();
        let mut xattrs = ping.listxattr().await.unwrap();
        xattrs.sort();
        assert_eq!(xattrs, [b"security.capability".to_vec(), b"user.origin".to_vec()]);
        assert_eq!(ping.getxattr(b"security.capability").await.unwrap(), capability);
        assert_eq!(ping.getxattr(b"user.origin").await.unwrap(), b"test");

        let home = attr_at(&fs, "home/user").await.unwrap();
        assert_eq!((home.perm, home.uid, home.gid), (0o700, 1000, 1000));
        let notes = attr_at(&fs, "home/user/notes").await.unwrap();
        assert_eq!((notes.perm, notes.uid, notes.gid, notes.nlink), (0o600, 1000, 100, 2));
        let hard = attr_at(&fs, "home/user/hard").await.unwrap();
        assert_eq!(hard.inode, notes.inode);
        let (link_node, link) = at(&fs, "home/user/link").await.unwrap();
        assert_eq!((link.file_type, link.uid, link.gid), (FileType::Symlink, 1000, 100));
        assert_eq!(link.mtime, time(MTIME));
        assert_eq!(link_node.readlink().await.unwrap(), b"notes");

        let null = attr_at(&fs, "dev/null").await.unwrap();
        assert_eq!((null.file_type, null.perm, null.rdev), (FileType::CharDevice, 0o666, 0x103));
        let sda1 = attr_at(&fs, "dev/sda1").await.unwrap();
        assert_eq!((sda1.file_type, sda1.rdev, sda1.gid), (FileType::BlockDevice, 0x801, 6));
        let fifo = attr_at(&fs, "run/fifo").await.unwrap();
        assert_eq!((fifo.file_type, fifo.perm), (FileType::Fifo, 0o620));

        // The zeros are holes: two pages hold data, the head's and the tail's.
        let (holey_file, holey_attr) = at(&fs, "var/holey").await.unwrap();
        assert_eq!(holey_attr.size, holey.len() as u64);
        assert_eq!(holey_attr.blocks * 512, 2 * u64::from(Storage::BLOCK_SIZE));
        assert_eq!(contents(&holey_file).await, holey);
        // A file of zeros is a hole.
        let (zeros_file, zeros) = at(&fs, "var/zeros").await.unwrap();
        assert_eq!((zeros.size, zeros.blocks), (128 << 10, 0));
        assert_eq!(contents(&zeros_file).await, [0; 128 << 10]);
        close(fs).await;
    })
}

/// A file longer than the importer reads of it at a time is written whole: its data, and its
/// holes, across the chunks it is read in too, and at its end.
#[test]
fn files_longer_than_a_chunk_are_written_whole() {
    const MIB: usize = 1 << 20;
    let mut long: Vec<u8> = (0..4 * MIB + 100).map(|at| (at % 251 + 1) as u8).collect();
    // A hole across the first MiB's end, which pages of 4 to 32 KiB are whole in, and one at the
    // end.
    long[MIB - (32 << 10)..MIB + (32 << 10)].fill(0);
    long.resize(5 * MIB + 100, 0);
    let layer = layer(&[Entry::new("long", Kind::File(&long))]).unwrap();
    block_on(async {
        let fs = imported(&[layer]).await;
        let (file, attr) = at(&fs, "long").await.unwrap();
        assert_eq!(attr.size, long.len() as u64);
        assert_eq!(contents(&file).await, long);
        // Of the data's blocks, the last holds 100 bytes.
        let block_size = u64::from(Storage::BLOCK_SIZE);
        assert_eq!(attr.blocks * 512, (4 << 20) - (64 << 10) + block_size);
        drop(file);
        close(fs).await;
    })
}

/// Runs `future` to completion on this thread, as `block_on` does, but fails as soon as the kernel
/// has failed an allocation for want of memory (`has_alloc_failed_no_mem`), rather than wait on a
/// future that may never complete: a write whose pages the kernel failed to allocate asked for them
/// again, and again (TASK-79.70).
fn unless_memory_fails<T>(future: impl Future<Output = T>) -> T {
    let mut future = pin!(future);
    block_on(poll_fn(|cx| {
        let failed = cladefs::slabs::has_alloc_failed_no_mem();
        assert!(!failed, "the kernel failed an allocation for want of memory");
        future.as_mut().poll(cx)
    }))
}

/// TASK-79.70: a file larger than the kernel's memory is imported whole, and read back: as its
/// pages fill memory, those written back are evicted for the rest, and, as it is read, for those
/// paged in. They stayed, and the import's writes waited for pages to dirty for ever.
#[test]
fn files_larger_than_the_kernels_memory_are_imported_whole() {
    const MEMORY: u64 = 64 << 20;
    cladefs::pages::configure(cladefs::pages::Config::with_memory(MEMORY)).unwrap();
    let large: Vec<u8> = (0..2 * MEMORY).map(|at| (at / 4096 % 251 + 1) as u8).collect();
    let layer = layer(&[Entry::new("large", Kind::File(&large))]).unwrap();
    unless_memory_fails(async {
        let fs = Filesystem::format(Storage::memory(4 * MEMORY)).await.unwrap();
        let mut importer = Importer::new(&fs);
        importer.apply(layer.as_slice()).await.expect("applying the layer failed");
        importer.finish().await.expect("finishing failed");
        let (file, attr) = at(&fs, "large").await.unwrap();
        assert_eq!(attr.size, large.len() as u64);
        let read = contents(&file).await;
        let differs = read.iter().zip(&large).position(|(read, written)| read != written);
        assert_eq!(differs, None, "the file read back differs from what was written");
        drop(file);
        close(fs).await;
    })
}

#[test]
fn layers_apply_in_turn_with_whiteouts() {
    let base = layer(&[
        Entry::new("app", Kind::Dir),
        Entry::new("app/config", Kind::File(b"base")),
        Entry::new("app/data", Kind::Dir),
        Entry::new("app/data/a", Kind::File(b"a")),
        Entry::new("app/data/b", Kind::File(b"b")),
        Entry::new("app/cache/old", Kind::File(b"old")),
        Entry::new("opaque", Kind::Dir),
        Entry::new("opaque/below", Kind::File(b"below")),
        Entry::new("becomes-dir", Kind::File(b"file")),
        Entry::new("becomes-file/inside", Kind::File(b"inside")),
        Entry::new("kept", Kind::Dir).perm(0o750),
        Entry::new("kept/child", Kind::File(b"child")),
    ])
    .unwrap();
    let top = layer(&[
        Entry::new("app/config", Kind::File(b"top")),
        Entry::new(&whiteout("app/data/a"), Kind::File(b"")),
        Entry::new(&whiteout("app/cache"), Kind::File(b"")),
        Entry::new(&whiteout("never/there"), Kind::File(b"")),
        Entry::new("opaque/above", Kind::File(b"above")),
        Entry::new(&opaque("opaque"), Kind::File(b"")),
        Entry::new("becomes-dir", Kind::Dir),
        Entry::new("becomes-file", Kind::File(b"now a file")),
        // A directory over a directory keeps its entries, and takes the new attributes.
        Entry::new("kept", Kind::Dir).perm(0o711).owner(5, 5).mtime(MTIME + 9),
    ])
    .unwrap();
    block_on(async {
        let fs = imported(&[base, top]).await;
        assert_eq!(contents(&at(&fs, "app/config").await.unwrap().0).await, b"top");
        assert_eq!(names(&fs, "app").await, ["config", "data"]);
        assert_eq!(names(&fs, "app/data").await, ["b"]);
        // An opaque whiteout keeps what its own layer made.
        assert_eq!(names(&fs, "opaque").await, ["above"]);
        assert_eq!(attr_at(&fs, "becomes-dir").await.unwrap().file_type, FileType::Directory);
        let (file, _) = at(&fs, "becomes-file").await.unwrap();
        assert_eq!(contents(&file).await, b"now a file");
        let kept = attr_at(&fs, "kept").await.unwrap();
        assert_eq!((kept.perm, kept.uid, kept.mtime), (0o711, 5, time(MTIME + 9)));
        assert_eq!(names(&fs, "kept").await, ["child"]);
        assert_eq!(names(&fs, "").await, ["app", "becomes-dir", "becomes-file", "kept", "opaque"]);
        assert_eq!(attr_at(&fs, "never").await.unwrap_err(), Errno::ENOENT);
        close(fs).await;
    })
}

/// What is removed is forgotten, with what was in it, but nothing else: not its neighbours whose
/// names begin with its name, `x-y`, which sorts between `x` and what is in it, and `x0`, which
/// sorts right after.
#[test]
fn removals_forget_what_they_remove_and_nothing_else() {
    let base = layer(&[
        Entry::new("o/below", Kind::File(b"below")),
        Entry::new("o/x/inner", Kind::File(b"inner")),
    ])
    .unwrap();
    let replacing = layer(&[
        Entry::new("o/x-y", Kind::Dir).mtime(MTIME + 1),
        Entry::new("o/x0", Kind::Dir).mtime(MTIME + 2),
        Entry::new("o/x/sub", Kind::Dir).mtime(MTIME + 3),
        // Replaces the directory, with what is in it.
        Entry::new("o/x", Kind::File(b"x")),
        // Removes what the layers below made in `o`, `below`, and nothing this one made.
        Entry::new(&opaque("o"), Kind::File(b"")),
    ])
    .unwrap();
    let again = layer(&[
        Entry::new("o/x", Kind::Dir),
        // In a new `sub`, not the one removed.
        Entry::new("o/x/sub/f", Kind::File(b"f")),
    ])
    .unwrap();
    block_on(async {
        let fs = imported(&[base, replacing, again]).await;
        assert_eq!(names(&fs, "o").await, ["x", "x-y", "x0"]);
        assert_eq!(attr_at(&fs, "o/x-y").await.unwrap().mtime, time(MTIME + 1));
        assert_eq!(attr_at(&fs, "o/x0").await.unwrap().mtime, time(MTIME + 2));
        assert_eq!(names(&fs, "o/x").await, ["sub"]);
        // Made where no entry made it: at the epoch.
        assert_eq!(attr_at(&fs, "o/x/sub").await.unwrap().mtime, UNIX_EPOCH);
        assert_eq!(contents(&at(&fs, "o/x/sub/f").await.unwrap().0).await, b"f");
        close(fs).await;
    })
}

/// TASK-79.98: a layer's whiteouts hide what the layers below made, not what the layer itself
/// makes, whatever the order of its entries: a file it replaces, a directory it makes something
/// in, or gives attributes to, under an opaque whiteout or not. Such a directory is as the layer
/// says, or as an implicit one, not as the layers below left it.
#[test]
fn whiteouts_hide_lower_layers_alone_in_any_order() {
    let lower = &[("user.lower", &b"1"[..])];
    let base = layer(&[
        Entry::new("x", Kind::File(b"lower x")),
        Entry::new("d/sub/old", Kind::File(b"old")),
        Entry::new("d/gone", Kind::File(b"gone")),
        Entry::new("d/again", Kind::Dir).perm(0o700).xattrs(lower),
        Entry::new("d/again/old", Kind::File(b"old")),
        Entry::new("passed", Kind::Dir).perm(0o700).xattrs(lower),
        Entry::new("passed/old", Kind::File(b"old")),
        Entry::new("again", Kind::Dir).perm(0o700).xattrs(lower),
        Entry::new("again/old", Kind::File(b"old")),
    ])
    .unwrap();
    let made = [
        Entry::new("x", Kind::File(b"upper x")),
        Entry::new("d/sub/new", Kind::File(b"new")),
        Entry::new("d/again", Kind::Dir).perm(0o711).mtime(MTIME + 3),
        Entry::new("d/again/new", Kind::File(b"new")),
        Entry::new("passed/new", Kind::File(b"new")),
        Entry::new("again", Kind::Dir).perm(0o711).owner(5, 5).mtime(MTIME + 7),
        Entry::new("again/new", Kind::File(b"new")),
    ];
    let names_whited_out = [whiteout("x"), opaque("d"), whiteout("passed"), whiteout("again")];
    let whiteouts = names_whited_out.each_ref().map(|name| Entry::new(name, Kind::File(b"")));
    let first = [&whiteouts[..], &made].concat();
    let last = [&made[..], &whiteouts].concat();
    let mut between = made.to_vec();
    for (at, whiteout) in whiteouts.into_iter().enumerate() {
        between.insert(2 * at + 1, whiteout);
    }
    let manifest_of = |entries: &[Entry]| {
        block_on(async {
            let fs = imported(&[base.clone(), layer(entries).unwrap()]).await;
            let manifest = manifest(&fs).await;
            close(fs).await;
            manifest
        })
    };
    block_on(async {
        let fs = imported(&[base.clone(), layer(&last).unwrap()]).await;
        assert_eq!(names(&fs, "").await, ["again", "d", "passed", "x"]);
        assert_eq!(contents(&at(&fs, "x").await.unwrap().0).await, b"upper x");
        assert_eq!(names(&fs, "d").await, ["again", "sub"]);
        assert_eq!(names(&fs, "d/sub").await, ["new"]);
        assert_eq!(names(&fs, "d/again").await, ["new"]);
        assert_eq!(names(&fs, "passed").await, ["new"]);
        assert_eq!(names(&fs, "again").await, ["new"]);
        let d_again = attr_at(&fs, "d/again").await.unwrap();
        assert_eq!((d_again.perm, d_again.mtime), (0o711, time(MTIME + 3)));
        let passed = attr_at(&fs, "passed").await.unwrap();
        assert_eq!((passed.perm, passed.uid, passed.mtime), (0o755, 0, UNIX_EPOCH));
        let again = attr_at(&fs, "again").await.unwrap();
        assert_eq!((again.perm, again.uid, again.mtime), (0o711, 5, time(MTIME + 7)));
        for path in ["d/again", "passed", "again"] {
            let xattrs = at(&fs, path).await.unwrap().0.listxattr().await.unwrap();
            assert_eq!(xattrs, Vec::<Vec<u8>>::new(), "{path}");
        }
        close(fs).await;
    });
    let expected = manifest_of(&first);
    assert_eq!(manifest_of(&last), expected);
    assert_eq!(manifest_of(&between), expected);
}

/// Everything about the nodes in `fs` that an image gives them, by path: all but their inode
/// numbers and change times.
async fn manifest(fs: &Filesystem) -> Vec<String> {
    let mut manifest = Vec::new();
    let mut dirs = vec![(String::new(), fs.root().clone())];
    while let Some((path, dir)) = dirs.pop() {
        for name in names(fs, &path).await {
            let path = format!("{path}/{name}");
            let (node, attr) = dir.lookup(name.as_bytes()).await.unwrap();
            let mut xattrs = node.listxattr().await.unwrap();
            xattrs.sort();
            let mut line = format!(
                "{path} {:?} {:o} {}:{} size={} nlink={} rdev={:x} mtime={:?} atime={:?}",
                attr.file_type,
                attr.perm,
                attr.uid,
                attr.gid,
                attr.size,
                attr.nlink,
                attr.rdev,
                attr.mtime,
                attr.atime,
            );
            for xattr in xattrs {
                let value = node.getxattr(&xattr).await.unwrap();
                line += &format!(" {}={value:?}", String::from_utf8_lossy(&xattr));
            }
            match attr.file_type {
                FileType::Directory => dirs.push((path, node)),
                FileType::Symlink => line += &format!(" -> {:?}", node.readlink().await),
                FileType::File => line += &format!(" {:?}", contents(&node).await),
                _ => {}
            }
            manifest.push(line);
        }
    }
    manifest.sort();
    manifest
}

/// Imports the image in `layout` into a new filesystem, and returns its manifest.
fn import_layout(layout: &Path, reference: Option<&str>) -> Vec<String> {
    block_on(async {
        let image = OciImage::open(layout, reference).expect("opening the image failed");
        let fs = Filesystem::format(Storage::memory(STORAGE_SIZE)).await.unwrap();
        let mut importer = Importer::new(&fs);
        image.import(&mut importer).await.expect("importing failed");
        importer.finish().await.unwrap();
        let manifest = manifest(&fs).await;
        close(fs).await;
        manifest
    })
}

fn sample_layers() -> Vec<Vec<u8>> {
    let base = layer(&[
        Entry::new("bin/sh", Kind::File(b"\x7fELF")).perm(0o755),
        Entry::new("bin/ping", Kind::File(b"ping")).xattrs(&[("user.a", b"1"), ("user.b", b"2")]),
        Entry::new("etc/hosts", Kind::File(b"127.0.0.1 localhost\n")).mtime(MTIME + 5),
        Entry::new("etc/alias", Kind::Symlink("hosts")).owner(3, 4),
        Entry::new("etc/again", Kind::HardLink("etc/hosts")),
        Entry::new("dev/zero", Kind::CharDevice(1, 5)),
        Entry::new("gone", Kind::File(b"gone")),
    ])
    .unwrap();
    let top = layer(&[
        Entry::new(&whiteout("gone"), Kind::File(b"")),
        Entry::new("etc/motd", Kind::File(b"hello\n")).owner(1, 2),
    ])
    .unwrap();
    vec![gzip(&base).unwrap(), top]
}

#[test]
fn the_same_image_gives_the_same_files() {
    let dir = tempfile::tempdir().unwrap();
    let layout = dir.path().join("image");
    oci_layout(&layout, "sample:1", &sample_layers()).unwrap();

    let first = import_layout(&layout, Some("sample:1"));
    let second = import_layout(&layout, None);
    assert_eq!(first, second);
    let line = |path: &str| first.iter().find(|line| line.starts_with(path)).cloned();
    assert_eq!(
        line("/etc/hosts").unwrap(),
        format!(
            "/etc/hosts File 644 0:0 size=20 nlink=2 rdev=0 mtime={:?} atime={:?} {:?}",
            time(MTIME + 5),
            time(MTIME + 5),
            b"127.0.0.1 localhost\n"
        )
    );
    assert!(line("/etc/motd").unwrap().starts_with("/etc/motd File 644 1:2 size=6"));
    assert!(line("/bin/ping").unwrap().contains(" user.a=[49] user.b=[50]"));
    assert_eq!(line("/gone"), None);
    assert_eq!(first.len(), 10, "{first:#?}");
}

#[test]
fn an_image_must_be_named_and_its_blobs_whole() {
    let dir = tempfile::tempdir().unwrap();
    let layout = dir.path().join("image");
    oci_layout(&layout, "sample:1", &sample_layers()).unwrap();
    let error = OciImage::open(&layout, Some("other:2")).err().expect("opened");
    assert_eq!(format!("{error:#}").rsplit(": ").next().unwrap(), "no image is named \"other:2\"");

    // A layer whose contents are not its digest's: the gzipped one.
    let gzipped: Vec<_> = std::fs::read_dir(layout.join("blobs/sha256"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| std::fs::read(path).unwrap().starts_with(&[0x1f, 0x8b]))
        .collect();
    let [base_digest] = gzipped.as_slice() else { panic!("gzipped layers: {gzipped:?}") };
    let mut corrupt = std::fs::read(base_digest).unwrap();
    *corrupt.last_mut().unwrap() ^= 1;
    std::fs::write(base_digest, corrupt).unwrap();
    block_on(async {
        let image = OciImage::open(&layout, None).unwrap();
        let fs = Filesystem::format(Storage::memory(STORAGE_SIZE)).await.unwrap();
        let mut importer = Importer::new(&fs);
        let error = image.import(&mut importer).await.expect_err("imported a corrupt layer");
        let hex = base_digest.file_name().unwrap().to_str().unwrap();
        assert!(
            format!("{error:#}").starts_with(&format!("the blob sha256:{hex} holds sha256:")),
            "{error:#}"
        );
        drop(importer);
        close(fs).await;
    })
}

/// Board 8541: a layer's blob is applied as it was hashed, opened once: a blob replaced in the
/// layout once hashed, before it is applied, as one who can write the layout as it is imported
/// may, is not what is applied. Until then the import opened it again by its path, and applied
/// whatever was there, its digest unchecked.
#[test]
fn a_layer_swapped_once_hashed_is_not_applied() {
    use cladefs::import::testing::import_hashed;
    let dir = tempfile::tempdir().unwrap();
    let layout = dir.path().join("image");
    let hashed = layer(&[Entry::new("hashed", Kind::File(b"as hashed"))]).unwrap();
    oci_layout(&layout, "sample:1", &[hashed]).unwrap();
    let swapped = layer(&[Entry::new("swapped", Kind::File(b"swapped in"))]).unwrap();
    let names = block_on(async {
        let image = OciImage::open(&layout, None).unwrap();
        let fs = Filesystem::format(Storage::memory(STORAGE_SIZE)).await.unwrap();
        let mut importer = Importer::new(&fs);
        let swap = |path: &Path| {
            let beside = path.with_extension("swapped");
            std::fs::write(&beside, &swapped).unwrap();
            std::fs::rename(&beside, path).unwrap();
        };
        import_hashed(&image, &mut importer, swap).await.unwrap();
        importer.finish().await.unwrap();
        let names = manifest(&fs).await;
        close(fs).await;
        names
    });
    assert_eq!(names.len(), 1, "{names:#?}");
    assert!(names[0].starts_with("/hashed File"), "{names:#?}");
}

/// Hotbed review 57 (board 8541): a layer's blob written in place once hashed, the file the
/// import holds open changed under it, is refused, `EINVAL`, never applied as if it were what was
/// hashed: a write of the same size, within the clock tick the blob was opened in, too, which
/// leaves its size and times as they were (board 9703), as what is applied is hashed as it is
/// read.
#[test]
fn a_layer_written_in_place_once_hashed_is_refused() {
    use cladefs::import::testing::import_hashed;
    use std::os::unix::fs::FileExt as _;
    let dir = tempfile::tempdir().unwrap();
    let layout = dir.path().join("image");
    let hashed = layer(&[Entry::new("hashed", Kind::File(b"as hashed"))]).unwrap();
    oci_layout(&layout, "sample:1", &[hashed.clone()]).unwrap();
    // A byte of the file it makes flipped in place, "as hashed" applied as "As hashed": its size
    // and times kept, as a write within the tick keeps them.
    let at = hashed.windows(9).position(|bytes| bytes == b"as hashed").unwrap();
    let flipped = [b'A'];
    let (at, len) = (at as u64, hashed.len() as u64);
    block_on(async {
        let image = OciImage::open(&layout, None).unwrap();
        let fs = Filesystem::format(Storage::memory(STORAGE_SIZE)).await.unwrap();
        let mut importer = Importer::new(&fs);
        let write = |path: &Path| {
            let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            let times = file.metadata().unwrap();
            file.write_all_at(&flipped, at).unwrap();
            let times = (times.modified().unwrap(), times.accessed().unwrap());
            file.set_times(std::fs::FileTimes::new().set_modified(times.0).set_accessed(times.1))
                .unwrap();
            assert_eq!(file.metadata().unwrap().len(), len);
        };
        let error = import_hashed(&image, &mut importer, write).await.expect_err("imported");
        let said = format!("{error:#}");
        assert!(said.contains("changed as it was imported"), "{said}");
        assert_eq!(error.errno(), Some(Errno::EINVAL), "{said}");
        drop(importer);
        close(fs).await;
    });
}

/// Board 8541: what an image's layout holds is opened as it is, never through a symlink, which
/// may name any file of the host, and `index.json`, a manifest and an index are refused past
/// what registries take (4 MiB) before they are read whole: each `EINVAL`, as a bad image is.
#[test]
fn linked_or_oversized_layout_files_are_refused_with_einval() {
    let dir = tempfile::tempdir().unwrap();
    let fresh = |name: &str| {
        let layout = dir.path().join(name);
        oci_layout(&layout, "sample:1", &sample_layers()).unwrap();
        layout
    };
    let blobs = |layout: &Path| -> Vec<std::path::PathBuf> {
        let listed = std::fs::read_dir(layout.join("blobs/sha256")).unwrap();
        listed.map(|entry| entry.unwrap().path()).collect()
    };
    // `path` a symlink to a copy of what it held, beside it.
    let linked = |path: &Path| {
        let copy = path.with_extension("copy");
        std::fs::rename(path, &copy).unwrap();
        std::os::unix::fs::symlink(&copy, path).unwrap();
    };
    let opening = |layout: &Path, expected: &str| {
        let error = OciImage::open(layout, None).err().expect("opened");
        let said = format!("{error:#}");
        assert!(said.contains(expected), "{expected:?} is not in {said:?}");
        assert_eq!(error.errno(), Some(Errno::EINVAL), "{said}");
    };

    let layout = fresh("linked-index");
    linked(&layout.join("index.json"));
    opening(&layout, "index.json is a symlink");

    // The manifest: its config names its layers' type, `"layers"`, too.
    let layout = fresh("linked-manifest");
    let manifest = blobs(&layout)
        .into_iter()
        .find(|blob| std::fs::read_to_string(blob).is_ok_and(|text| text.contains("\"layers\":")));
    let manifest = manifest.expect("a manifest");
    linked(&manifest);
    opening(&layout, "is a symlink");

    // A FIFO, which nothing writes: refused at once, never waited on for a writer (hotbed
    // review 57).
    let layout = fresh("fifo-index");
    std::fs::remove_file(layout.join("index.json")).unwrap();
    let fifo = std::ffi::CString::new(layout.join("index.json").into_os_string().into_vec());
    // SAFETY: mkfifo(3), with a NUL-terminated path that outlives the call.
    assert_eq!(unsafe { libc::mkfifo(fifo.unwrap().as_ptr(), 0o644) }, 0);
    opening(&layout, "index.json is not a regular file");

    let layout = fresh("large-index");
    let mut index = std::fs::read(layout.join("index.json")).unwrap();
    index.resize((4 << 20) + 1, b' ');
    std::fs::write(layout.join("index.json"), index).unwrap();
    opening(&layout, "index.json is larger than 4194304 bytes");

    // A manifest whose digest is its own, larger than 4 MiB.
    let layout = fresh("large-manifest");
    let mut large = vec![b' '; 4 << 20];
    large.extend_from_slice(b"{}");
    let hex: String = {
        use sha2::Digest as _;
        sha2::Sha256::digest(&large).iter().map(|byte| format!("{byte:02x}")).collect()
    };
    std::fs::write(layout.join("blobs/sha256").join(&hex), &large).unwrap();
    let index = format!(r#"{{"manifests":[{{"digest":"sha256:{hex}"}}]}}"#);
    std::fs::write(layout.join("index.json"), index).unwrap();
    opening(&layout, "is larger than 4194304 bytes");

    // A layer's blob: refused as it is imported.
    let layout = fresh("linked-layer");
    let image = OciImage::open(&layout, None).unwrap();
    for blob in blobs(&layout) {
        if std::fs::read(&blob).unwrap().starts_with(&[0x1f, 0x8b]) {
            linked(&blob);
        }
    }
    block_on(async {
        let fs = Filesystem::format(Storage::memory(STORAGE_SIZE)).await.unwrap();
        let mut importer = Importer::new(&fs);
        let error = image.import(&mut importer).await.expect_err("imported a linked layer");
        let said = format!("{error:#}");
        assert!(said.contains("is a symlink"), "{said}");
        assert_eq!(error.errno(), Some(Errno::EINVAL), "{said}");
        drop(importer);
        close(fs).await;
    });
}

/// A layer compressed with zstd is refused, saying so.
#[test]
fn zstd_layers_are_refused() {
    block_on(async {
        let fs = Filesystem::format(Storage::memory(STORAGE_SIZE)).await.unwrap();
        let mut importer = Importer::new(&fs);
        let layer = [0x28, 0xb5, 0x2f, 0xfd, 0, 0, 0, 0];
        let error = importer.apply(&layer[..]).await.expect_err("applied");
        assert_eq!(
            format!("{error:#}"),
            "zstd-compressed layers are not supported: EINVAL (errno 22)"
        );
        assert_eq!(error.errno(), Some(Errno::EINVAL));
        drop(importer);
        close(fs).await;
    })
}

/// Board 2526: an entry's path longer than `PATH_MAX` less its NUL is refused, `EINVAL`, saying
/// so: one of 60 KB, a PAX record's, made the importer's memory quadratic in its depth. One as
/// long as that is applied.
#[test]
fn paths_past_path_max_are_refused() {
    let deep = |components: usize| ["a/"].repeat(components).concat() + "f";
    block_on(async {
        let fs = Filesystem::format(Storage::memory(STORAGE_SIZE)).await.unwrap();
        let mut importer = Importer::new(&fs);
        let longest = deep(2047);
        assert_eq!(longest.len(), 4095);
        importer.apply(layer_of(&longest).as_slice()).await.unwrap();
        let error = importer.apply(layer_of(&deep(2048)).as_slice()).await.expect_err("applied");
        assert_eq!(error.errno(), Some(Errno::EINVAL), "{error:#}");
        assert!(format!("{error:#}").contains("a path of 4097 bytes, past 4095"), "{error:#}");
        drop(importer);
        close(fs).await;
    })
}

/// A layer of one file at `path`, of any length: its PAX record's.
fn layer_of(path: &str) -> Vec<u8> {
    layer(&[Entry::new(path, Kind::File(b"deep"))]).unwrap()
}

#[test]
fn paths_stay_in_the_image() {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(3);
    header.set_mode(0o644);
    // `append_data` refuses `..`, as importing does; the name is written as is.
    header.as_gnu_mut().unwrap().name[..11].copy_from_slice(b"../escaped\0");
    header.set_cksum();
    let mut builder = tar::Builder::new(Vec::new());
    builder.append(&header, &b"out"[..]).unwrap();
    let layer = builder.into_inner().unwrap();
    block_on(async {
        let fs = Filesystem::format(Storage::memory(STORAGE_SIZE)).await.unwrap();
        let mut importer = Importer::new(&fs);
        let error = importer.apply(layer.as_slice()).await.expect_err("applied");
        assert_eq!(
            format!("{error:#}"),
            "applying \"../escaped\": the path \"../escaped\" goes through ..: EINVAL (errno 22)"
        );
        assert_eq!(error.errno(), Some(Errno::EINVAL));
        drop(importer);
        close(fs).await;
    })
}

/// A layer of one entry, `name`, of `kind`, its header as `make` leaves it.
fn crafted(name: &str, kind: tar::EntryType, make: impl FnOnce(&mut tar::Header)) -> Vec<u8> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(kind);
    header.set_size(0);
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_path(name).unwrap();
    make(&mut header);
    header.set_cksum();
    let mut builder = tar::Builder::new(Vec::new());
    builder.append(&header, std::io::empty()).unwrap();
    builder.into_inner().unwrap()
}

/// A layer of a device node in a header of the old format, which has no device numbers.
fn old_device() -> Vec<u8> {
    let mut header = tar::Header::new_old();
    header.set_entry_type(tar::EntryType::Char);
    header.set_size(0);
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_path("dev").unwrap();
    header.set_cksum();
    let mut builder = tar::Builder::new(Vec::new());
    builder.append(&header, std::io::empty()).unwrap();
    builder.into_inner().unwrap()
}

/// A file's layer, with the PAX record `key` of `value`.
fn with_pax(key: &str, value: &[u8]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    builder.append_pax_extensions([(key, value)]).unwrap();
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(0);
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_path("file").unwrap();
    header.set_cksum();
    builder.append(&header, std::io::empty()).unwrap();
    builder.into_inner().unwrap()
}

/// TASK-79.88: each fault of a crafted image is `EINVAL`, not the `EIO` a failed host gives, each
/// saying what it is.
#[test]
fn crafted_images_are_refused_with_einval() {
    use tar::EntryType;
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "the owner's uid or gid is too large",
            crafted("f", EntryType::Regular, |h| {
                h.set_uid(1 << 40);
            }),
        ),
        (
            "the owner's uid or gid is too large",
            crafted("f", EntryType::Regular, |h| {
                h.set_gid(1 << 40);
            }),
        ),
        ("failed to parse pax uid", with_pax("uid", b"root")),
        ("failed to parse pax gid", with_pax("gid", b"root")),
        ("the PAX time", with_pax("mtime", b"noon")),
        ("the PAX time", with_pax("atime", b"1.5x")),
        ("no device major number", old_device()),
        (
            "is too large",
            crafted("dev", EntryType::Char, |h| {
                h.set_device_major(1 << 12).unwrap();
                h.set_device_minor(0).unwrap();
            }),
        ),
        (
            "is too large",
            crafted("dev", EntryType::Block, |h| {
                h.set_device_major(0).unwrap();
                h.set_device_minor(1 << 20).unwrap();
            }),
        ),
        ("no link target", crafted("link", EntryType::Link, |_| {})),
        (
            "the link's target is missing",
            crafted("link", EntryType::Link, |h| {
                h.set_link_name("nowhere").unwrap();
            }),
        ),
        ("no symlink target", crafted("link", EntryType::Symlink, |_| {})),
        ("are not supported", crafted("x", EntryType::new(b'X'), |_| {})),
        ("the root is not a directory", crafted("./", EntryType::Regular, |_| {})),
    ];
    for (expected, layer) in cases {
        block_on(async {
            let fs = Filesystem::format(Storage::memory(STORAGE_SIZE)).await.unwrap();
            let mut importer = Importer::new(&fs);
            let error = importer.apply(layer.as_slice()).await.expect_err(expected);
            let said = format!("{error:#}");
            assert!(said.contains(expected), "{expected:?} is not in {said:?}");
            assert_eq!(error.errno(), Some(Errno::EINVAL), "{said}");
            drop(importer);
            close(fs).await;
        })
    }
}

/// An entry whose time is past what a `SystemTime` can hold is refused with `EINVAL`, saying so,
/// as one past what Fxfs's nanoseconds hold is: its seconds since the Unix epoch, which a GNU
/// header holds in 64 bits, overflowed the time, and the import panicked.
#[test]
fn times_out_of_range_are_refused() {
    let layer = layer(&[Entry::new("file", Kind::File(b"f")).mtime(u64::MAX)]).unwrap();
    block_on(async {
        let fs = Filesystem::format(Storage::memory(STORAGE_SIZE)).await.unwrap();
        let mut importer = Importer::new(&fs);
        let error = importer.apply(layer.as_slice()).await.expect_err("applied");
        assert_eq!(format!("{error:#}"), format!("applying \"file\": {OUT_OF_RANGE}"));
        assert_eq!(error.errno(), Some(Errno::EINVAL));
        drop(importer);
        close(fs).await;
    })
}

/// What importing an entry made at `u64::MAX` seconds since the Unix epoch fails with.
const OUT_OF_RANGE: &str =
    "the time 18446744073709551615s since the Unix epoch is out of range: EINVAL (errno 22)";

/// A cell that cannot be made whole is deleted, leaving its path free for another: an image
/// with a time out of range left it half made, and every import to the path after it failed.
#[test]
fn a_cell_that_cannot_be_made_whole_is_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let image = |name: &str, entry: Entry<'_>| {
        let path = dir.path().join(name);
        std::fs::write(&path, layer(&[entry]).unwrap()).unwrap();
        path
    };
    let out_of_range = image("out-of-range.tar", Entry::new("f", Kind::File(b"f")).mtime(u64::MAX));
    let whole = image("whole.tar", Entry::new("f", Kind::File(b"f")));
    let cell_dir = dir.path().join("store");

    let made = block_on(cladefs::import::make_base(&cell_dir, STORAGE_SIZE, &out_of_range, None));
    let error = made.expect_err("made a cell of an image with a time out of range");
    assert_eq!(format!("{error:#}"), format!("applying \"f\": {OUT_OF_RANGE}"));
    assert_eq!(error.errno(), Some(Errno::EINVAL));
    assert!(!cell_dir.exists(), "the cell is left half made");
    block_on(cladefs::import::make_base(&cell_dir, STORAGE_SIZE, &whole, None))
        .expect("making failed");
    let line =
        format!("/f File 644 0:0 size=1 nlink=1 rdev=0 mtime={0:?} atime={0:?} [102]", time(MTIME));
    assert_eq!(cell_manifest(&cell_dir), [line]);
}

/// An image a thousand directories deep makes a cell on a thread with the stack an extent
/// daemon's blocking threads have, tokio's 2 MiB, though its whiteout removes a tree as deep:
/// removal kept a poll frame on the thread's stack for each directory it was emptying, and
/// overflowed it, aborting the process.
#[test]
fn deep_images_make_cells_on_a_daemons_thread() {
    const DEPTH: usize = 1000;
    const STACK: usize = 2 << 20;
    let deep = vec!["d"; DEPTH].join("/");
    let base = layer(&[
        Entry::new(&format!("{deep}/file"), Kind::File(b"removed")),
        Entry::new(&format!("kept/{deep}/file"), Kind::File(b"kept")),
    ])
    .unwrap();
    let top = layer(&[Entry::new(&whiteout("d"), Kind::File(b""))]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let layout = dir.path().join("image");
    oci_layout(&layout, "deep:1", &[base, top]).unwrap();
    let cell_dir = dir.path().join("store");
    std::thread::scope(|scope| {
        let made = std::thread::Builder::new().stack_size(STACK).spawn_scoped(scope, || {
            block_on(cladefs::import::make_base(&cell_dir, STORAGE_SIZE, &layout, Some("deep:1")))
        });
        made.unwrap().join().unwrap().expect("making the cell failed");
    });
    block_on(async {
        let fs = Filesystem::mount(Storage::open(&cell_dir).await.unwrap()).await.unwrap();
        assert_eq!(names(&fs, "").await, ["kept"]);
        let (file, _) = at(&fs, &format!("kept/{deep}/file")).await.unwrap();
        assert_eq!(contents(&file).await, b"kept");
        drop(file);
        close(fs).await;
    })
}

/// TASK-541 (board 2478): both a direct whiteout (`.wh.d`) and an opaque whiteout
/// (`d/.wh..wh..opq`) over a deep kept directory chain (where the upper layer creates a file
/// inside the deep chain before or alongside the whiteout) succeed on a 2 MiB daemon thread stack
/// without overflowing the stack during `remove_lower` or `Mounted::let_go`.
#[test]
fn deep_whiteout_over_kept_chain_succeeds_on_a_daemons_thread() {
    const DEPTH: usize = 512;
    const STACK: usize = 2 << 20;
    let deep = vec!["d"; DEPTH].join("/");
    let base = layer(&[
        Entry::new(&format!("{deep}/old"), Kind::File(b"old")),
        Entry::new("d/sibling", Kind::File(b"sibling")),
    ])
    .unwrap();
    // Upper layer creates a new file at the bottom of `deep` (marking every ancestor in `kept`)
    // AND applies a whiteout over `d` (then in case 2, an opaque whiteout `d/.wh..wh..opq`).
    let cases = [
        (
            "direct",
            layer(&[
                Entry::new(&format!("{deep}/new"), Kind::File(b"new")),
                Entry::new(&whiteout("d"), Kind::File(b"")),
            ])
            .unwrap(),
        ),
        (
            "opaque",
            layer(&[
                Entry::new(&format!("{deep}/new"), Kind::File(b"new")),
                Entry::new("d/.wh..wh..opq", Kind::File(b"")),
            ])
            .unwrap(),
        ),
    ];
    for (label, top) in cases {
        let dir = tempfile::tempdir().unwrap();
        let layout = dir.path().join("image");
        oci_layout(&layout, "wh:1", &[base.clone(), top]).unwrap();
        let cell_dir = dir.path().join("store");
        std::thread::scope(|scope| {
            let made = std::thread::Builder::new().stack_size(STACK).spawn_scoped(scope, || {
                block_on(cladefs::import::make_base(&cell_dir, STORAGE_SIZE, &layout, Some("wh:1")))
            });
            made.unwrap()
                .join()
                .unwrap()
                .unwrap_or_else(|err| panic!("{label} deep whiteout failed: {err:#}"));
        });
        block_on(async {
            let fs = Filesystem::mount(Storage::open(&cell_dir).await.unwrap()).await.unwrap();
            let (file, _) = at(&fs, &format!("{deep}/new")).await.unwrap();
            assert_eq!(contents(&file).await, b"new", "{label}");
            drop(file);
            assert_eq!(at(&fs, &format!("{deep}/old")).await.err(), Some(Errno::ENOENT), "{label}");
            assert_eq!(at(&fs, "d/sibling").await.err(), Some(Errno::ENOENT), "{label}");
            close(fs).await;
        });
    }
}

/// A cell made of an image where no extent daemon is, as `cladefsctl import` makes one, forks once
/// opened there too, as fxfs-guest-import forks the base it imports for its guest: the fork is
/// the bucket's alone (TASK-165.7), made by the cell's uploader, which keeps its cell in the
/// bucket its description names, its own beside it, as an image's does. Opened so, it had none,
/// and the fork failed (`ENOTSUP`, GCE r701). The fork holds the image's files, read from the
/// bucket, and the base is as it was.
#[tokio::test(flavor = "multi_thread")]
async fn cells_made_of_images_fork_where_no_daemon_is() {
    let dir = tempfile::tempdir().unwrap();
    let layout = dir.path().join("image");
    let image = layer(&[Entry::new("etc/motd", Kind::File(b"forked\n"))]).unwrap();
    oci_layout(&layout, "fork:1", &[image]).unwrap();
    let base = dir.path().join("base");
    block_on(cladefs::import::make_base(&base, STORAGE_SIZE, &layout, Some("fork:1"))).unwrap();
    let manifest = cell_manifest(&base);
    let fork = dir.path().join("fork");
    let base_cell = cladefs::OpenCell::open(&base).await.unwrap();
    base_cell.fork(&fork, base_cell.cell().tenant).await.unwrap();
    drop(base_cell);
    assert_eq!(cell_manifest(&fork), manifest);
    assert_eq!(cell_manifest(&base), manifest);
}

/// Runs cladefsctl with `args` through the daemon at `control`, and returns whether it succeeded,
/// what it printed, and what it said on stderr.
#[cfg(target_os = "linux")]
fn cladefsctl(control: &Path, args: &[&str]) -> (bool, String, String) {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_cladefsctl"));
    command.args(args).env("CLADEFSD_GRPC", control).env_remove("CLADEFS_CONFIG");
    let output = command.output().expect("running cladefsctl failed");
    let said = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    (output.status.success(), said(&output.stdout), said(&output.stderr))
}

/// The manifest of the filesystem in the cell at `dir`.
fn cell_manifest(dir: &Path) -> Vec<String> {
    block_on(async {
        let fs = Filesystem::mount(Storage::open(dir).await.unwrap()).await.unwrap();
        let manifest = manifest(&fs).await;
        close(fs).await;
        manifest
    })
}

/// `cladefsctl import` makes cells of images, through the daemon, which owns its host's cells
/// (board 14305; into a directory, offline, before): an OCI layout's, and a `docker export`
/// tarball's; one that can't be imported leaves no cell behind.
#[cfg(target_os = "linux")]
#[test]
fn cladefsctl_makes_cells_of_images() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = common::daemon::Daemon::new(dir.path());
    let control = dir.path().join("control.sock");
    let layout = dir.path().join("image");
    let layers = sample_layers();
    oci_layout(&layout, "sample:1", &layers).unwrap();
    let path = |name: &str| dir.path().join(name).to_str().unwrap().to_owned();
    let imported = |said: &str| daemon.cell_path(said.trim().parse().unwrap());

    let (ok, said, stderr) =
        cladefsctl(&control, &["import", "67108864", &path("image"), "sample:1"]);
    assert!(ok, "{stderr}");
    assert_eq!(cell_manifest(&imported(&said)), import_layout(&layout, None));

    // A `docker export` tarball: one layer.
    std::fs::write(path("export.tar"), &layers[0]).unwrap();
    let (ok, said, stderr) = cladefsctl(&control, &["import", "67108864", &path("export.tar")]);
    assert!(ok, "{stderr}");
    let manifest = cell_manifest(&imported(&said));
    assert_eq!(manifest.iter().filter(|line| line.starts_with("/gone File")).count(), 1);
    assert_eq!(manifest.len(), 10, "{manifest:#?}");

    // An image that cannot be imported leaves no cell behind.
    let tenant = dir.path().join("disk0/cells").join(base32_id::Padded(0).to_string());
    let cells = || std::fs::read_dir(&tenant).map_or(0, |cells| cells.count());
    let before = cells();
    let (ok, _, stderr) = cladefsctl(&control, &["import", "67108864", &path("image"), "x:1"]);
    assert!(!ok);
    // Board 14314: the daemon's reason (`no image is named "x:1"`) stays in its log, the call
    // failing EIO alone; checked here again once the control API carries it.
    assert!(stderr.contains(&format!("importing {}", path("image"))), "{stderr}");
    assert_eq!(cells(), before, "a cell left behind");
}
