// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: Fxfs's golden corpus (doc-28, board 10050): pins `EARLIEST_SUPPORTED_VERSION` at the
//! frozen version (66), stores a compressed golden image (files, nested directories, hard links,
//! extended attributes, symlinks, special files, and a shared base's layers and manifest) together
//! with `get_type_fingerprints`, and verifies every later build mounts, fscks, and reads it back.

use crate::tests::Tested;
use crate::{Fxfs, ReaddirStart, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFREG, S_IFSOCK};
use bincode::Options as _;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use fprint::TypeFingerprint as _;
use fxfs::errors::FxfsError;
use fxfs::filesystem::{FxFilesystem, FxFilesystemBuilder, SyncOptions};
use fxfs::fsck::{FsckOptions, fsck_volume_with_options, fsck_with_options};
use fxfs::object_store::journal::super_block::Root;
use fxfs::object_store::volume::root_volume;
use fxfs::object_store::{NewChildStoreOptions, SetExtendedAttributeMode};
use fxfs::serialized_types::{
    EARLIEST_SUPPORTED_VERSION, LATEST_VERSION, Version, get_type_fingerprints,
};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use storage_device::fake_device::{Crashes, FakeDevice};
use storage_device::{DeviceHolder, ExtentId};

/// cladefs: Fxfs's frozen format range (doc-28, board 10050), declared with `formats::Format` so
/// `scripts/format-freeze.py` guards `EARLIEST_SUPPORTED_VERSION` and `LATEST_VERSION` alongside
/// every other frozen family.
const FORMAT: formats::Format = formats::Format::new("fxfs", 66, 66);

const _: () = {
    assert!(FORMAT.oldest == EARLIEST_SUPPORTED_VERSION.major);
    assert!(FORMAT.current == LATEST_VERSION.major);
    assert!(EARLIEST_SUPPORTED_VERSION.minor == 0);
    assert!(LATEST_VERSION.minor == 0);
};

const WRITE_ENV: &str = "CLADEFS_FORMATS_WRITE";
const BLOCK_SIZE: u32 = 4096;
const CAPACITY: u64 = 4 << 20;
const BASE_CELL: u64 = 10;
const BASE_GENERATION: u64 = 11;
const CHILD_CELL: u64 = 20;
const FILE_MODE: u32 = S_IFREG | 0o644;
const DIR_MODE: u32 = S_IFDIR | 0o755;

#[derive(Serialize, Deserialize)]
struct GoldenImage {
    block_size: u32,
    capacity: u64,
    base_cell: u64,
    base_extents: Vec<(ExtentId, Vec<u8>)>,
    base_root: Vec<u8>,
    child_cell: u64,
    child_extents: Vec<(ExtentId, Vec<u8>)>,
    child_root: Vec<u8>,
}

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|dir| dir.join("Cargo.lock").is_file())
        .expect("a crate is in a workspace")
        .join("formats")
        .join(FORMAT.slug())
}

fn render_fingerprints(major: u32) -> String {
    let mut map = get_type_fingerprints(Version { major, minor: 0 });
    map.insert("Root".to_owned(), Root::fingerprint());
    let mut out = String::new();
    for (name, fp) in map {
        writeln!(out, "{name} = {fp}").expect("writing to a string");
    }
    out
}

async fn read_whole(fs: &Fxfs<Tested>, ino: u64) -> Vec<u8> {
    let size = fs.attr(ino).await.unwrap().size as usize;
    let mut buf = vec![0u8; size];
    let mut offset = 0usize;
    while offset < size {
        let n = fs.read(ino, offset as u64, &mut buf[offset..]).await.unwrap();
        assert!(n > 0, "short read on inode {ino} at {offset}/{size}");
        offset += n;
    }
    buf
}

async fn list_dir(fs: &Fxfs<Tested>, dir: u64) -> Vec<(String, u32)> {
    let mut entries = Vec::new();
    fs.readdir(dir, ReaddirStart::Index(0), |name, _, kind| {
        entries.push((name.to_owned(), kind));
        true
    })
    .await
    .unwrap();
    entries
}

async fn close_and_snapshot(
    filesystem: fxfs::filesystem::OpenFxFilesystem,
    crashes: &Crashes,
) -> (Vec<(ExtentId, Vec<u8>)>, Vec<u8>) {
    filesystem.sync(SyncOptions { flush_device: true, ..Default::default() }).await.unwrap();
    filesystem.close().await.unwrap();
    let device = filesystem.take_device().await;
    device.reopen(false);
    crashes.mark(0);
    crashes.last();
    device.flush().await.unwrap();
    let (_, snapshot) = crashes.take().next().expect("snapshot captured");
    snapshot.extents_snapshot()
}

async fn build_golden() -> (Vec<u8>, Vec<u8>) {
    // 1. Base filesystem holding volume "base" with files, nested dirs, xattrs, symlink, FIFO.
    let base_crashes = Crashes::new(0, u32::MAX);
    let base_dev = FakeDevice::new(CAPACITY / u64::from(BLOCK_SIZE), BLOCK_SIZE)
        .calm()
        .with_cell(BASE_CELL)
        .record_crashes(base_crashes.clone());
    let base_fs = FxFilesystem::new_empty(DeviceHolder::new(base_dev)).await.unwrap();
    let base_root_vol = root_volume((*base_fs).clone()).await.unwrap();
    let base_store =
        base_root_vol.new_volume("base", NewChildStoreOptions::default()).await.unwrap();
    let base_root_dir = base_store.root_directory_object_id();
    let base_posix = Fxfs::new((*base_fs).clone(), base_store.clone(), false, Tested);

    let hello = base_posix
        .create(base_root_dir, b"hello.txt", FILE_MODE, 1000, 1000, 0, &[])
        .await
        .unwrap()[0]
        .ino;
    base_posix.write(hello, 0, b"hello from base\n").await.unwrap();
    base_posix
        .set_xattr(hello, b"user.base", b"base-xattr", SetExtendedAttributeMode::Create)
        .await
        .unwrap();
    base_posix
        .set_xattr(hello, b"user.shared", b"from-base", SetExtendedAttributeMode::Create)
        .await
        .unwrap();

    let dir =
        base_posix.create(base_root_dir, b"dir", DIR_MODE, 0, 0, 0, &[]).await.unwrap()[0].ino;
    let nested = base_posix.create(dir, b"nested", DIR_MODE, 0, 0, 0, &[]).await.unwrap()[0].ino;
    let deep =
        base_posix.create(nested, b"deep.bin", FILE_MODE, 0, 0, 0, &[]).await.unwrap()[0].ino;
    base_posix.write(deep, 0, &[0xab; 12_288]).await.unwrap();

    let over =
        base_posix.create(base_root_dir, b"override.txt", FILE_MODE, 0, 0, 0, &[]).await.unwrap()
            [0]
        .ino;
    base_posix.write(over, 0, b"base version").await.unwrap();

    let gone =
        base_posix.create(base_root_dir, b"remove-me.txt", FILE_MODE, 0, 0, 0, &[]).await.unwrap()
            [0]
        .ino;
    base_posix.write(gone, 0, b"to be unlinked in child").await.unwrap();

    base_posix.symlink(base_root_dir, b"link-to-hello", b"hello.txt", 1000, 1000).await.unwrap();
    base_posix.create(base_root_dir, b"pipe", S_IFIFO | 0o600, 0, 0, 0, &[]).await.unwrap();

    base_posix.stop_delayed().await.unwrap();
    drop(base_posix);
    drop(base_store);
    // Compact base_fs before publishing so AllocatorInfo, allocator layers, and root_store layers
    // are persisted alongside the published base volume layer (hotbed review 98, finding 339).
    base_fs.journal().force_compact().await.unwrap();
    let manifest = base_root_vol.publish_volume("base").await.unwrap();
    drop(base_root_vol);
    base_fs.sync(SyncOptions { flush_device: true, ..Default::default() }).await.unwrap();

    // 2. Child filesystem attached to a read-through fork of base: holds both a standalone volume
    //    "main" and a shared-base volume "over-base" created from `manifest`.
    let child_crashes = Crashes::new(0, u32::MAX);
    let mut child_dev = FakeDevice::new(CAPACITY / u64::from(BLOCK_SIZE), BLOCK_SIZE)
        .calm()
        .with_cell(CHILD_CELL)
        .record_crashes(child_crashes.clone());
    child_dev.attach(base_fs.device().fork(BASE_GENERATION).unwrap());
    let child_fs = FxFilesystem::new_empty(DeviceHolder::new(child_dev)).await.unwrap();
    let child_root_vol = root_volume((*child_fs).clone()).await.unwrap();

    // Standalone volume "main".
    let main_store =
        child_root_vol.new_volume("main", NewChildStoreOptions::default()).await.unwrap();
    let main_root = main_store.root_directory_object_id();
    let main_posix = Fxfs::new((*child_fs).clone(), main_store.clone(), false, Tested);

    let data =
        main_posix.create(main_root, b"data.bin", FILE_MODE, 0, 0, 0, &[]).await.unwrap()[0].ino;
    main_posix.write(data, 0, &[0x5a; 8192]).await.unwrap();
    main_posix
        .set_xattr(data, b"user.tag", b"golden-v66", SetExtendedAttributeMode::Create)
        .await
        .unwrap();
    main_posix.create(main_root, b"empty", FILE_MODE, 0, 0, 0, &[]).await.unwrap();

    let sub = main_posix.create(main_root, b"sub", DIR_MODE, 0, 0, 0, &[]).await.unwrap()[0].ino;
    let leaf = main_posix.create(sub, b"leaf", DIR_MODE, 0, 0, 0, &[]).await.unwrap()[0].ino;
    let note = main_posix.create(leaf, b"note.txt", FILE_MODE, 0, 0, 0, &[]).await.unwrap()[0].ino;
    main_posix.write(note, 0, b"nested note").await.unwrap();
    main_posix.link(note, sub, b"hardlink.txt", false).await.unwrap();
    main_posix.symlink(sub, b"rel-link", b"leaf/note.txt", 0, 0).await.unwrap();
    main_posix.create(sub, b"sock", S_IFSOCK | 0o600, 0, 0, 0, &[]).await.unwrap();
    main_posix.stop_delayed().await.unwrap();
    drop(main_posix);
    drop(main_store);

    // Shared-base volume "over-base".
    let over_store = child_root_vol.new_volume_over_base("over-base", &manifest).await.unwrap();
    let over_root = over_store.root_directory_object_id();
    let over_posix = Fxfs::new((*child_fs).clone(), over_store.clone(), false, Tested);

    let over_file = over_posix.lookup(over_root, b"override.txt").await.unwrap().ino;
    over_posix.write(over_file, 0, b"child version").await.unwrap();
    let rm_ino = over_posix.lookup(over_root, b"remove-me.txt").await.unwrap().ino;
    over_posix.unlink(over_root, b"remove-me.txt", rm_ino).await.unwrap();
    let child_only =
        over_posix.create(over_root, b"child-only.txt", FILE_MODE, 0, 0, 0, &[]).await.unwrap()[0]
            .ino;
    over_posix.write(child_only, 0, b"only in child").await.unwrap();
    over_posix
        .set_xattr(child_only, b"user.child", b"child-xattr", SetExtendedAttributeMode::Create)
        .await
        .unwrap();
    over_posix.symlink(over_root, b"child-link", b"child-only.txt", 0, 0).await.unwrap();
    over_posix.stop_delayed().await.unwrap();
    drop(over_posix);
    drop(over_store);

    // Force a full compaction of child_fs so AllocatorInfo, allocator layers, root_store layers,
    // and both child volume layers ("main" and "over-base") are persisted as layer files, then
    // write one post-compaction file so journal replay is exercised too (hotbed review 98, #339).
    child_fs.journal().force_compact().await.unwrap();
    let main_store = child_root_vol.volume("main").await.unwrap();
    let main_root = main_store.root_directory_object_id();
    let main_posix = Fxfs::new((*child_fs).clone(), main_store.clone(), false, Tested);
    let journaled =
        main_posix.create(main_root, b"journaled.txt", FILE_MODE, 0, 0, 0, &[]).await.unwrap()[0]
            .ino;
    main_posix.write(journaled, 0, b"replayed from journal").await.unwrap();
    main_posix.stop_delayed().await.unwrap();
    drop(main_posix);
    drop(main_store);
    drop(child_root_vol);

    let (child_extents, child_root) = close_and_snapshot(child_fs, &child_crashes).await;
    let (base_extents, base_root) = close_and_snapshot(base_fs, &base_crashes).await;

    let image = GoldenImage {
        block_size: BLOCK_SIZE,
        capacity: CAPACITY,
        base_cell: BASE_CELL,
        base_extents,
        base_root,
        child_cell: CHILD_CELL,
        child_extents,
        child_root,
    };
    let raw = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .serialize(&image)
        .expect("golden image serializes");
    let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&raw).expect("gzip write");
    let compressed = encoder.finish().expect("gzip finish");
    (compressed, manifest)
}

async fn verify_base_volume(fs: &Fxfs<Tested>, root: u64) {
    assert_eq!(
        list_dir(fs, root).await,
        vec![
            ("dir".into(), S_IFDIR),
            ("hello.txt".into(), S_IFREG),
            ("link-to-hello".into(), S_IFLNK),
            ("override.txt".into(), S_IFREG),
            ("pipe".into(), S_IFIFO),
            ("remove-me.txt".into(), S_IFREG),
        ]
    );
    let hello = fs.lookup(root, b"hello.txt").await.unwrap().ino;
    assert_eq!(read_whole(fs, hello).await, b"hello from base\n");
    let mut xattrs = fs.list_xattrs(hello).await.unwrap();
    xattrs.sort();
    assert_eq!(xattrs, vec![b"user.base".to_vec(), b"user.shared".to_vec()]);
    assert_eq!(fs.get_xattr(hello, b"user.base").await.unwrap(), b"base-xattr");
    assert_eq!(fs.get_xattr(hello, b"user.shared").await.unwrap(), b"from-base");

    let dir = fs.lookup(root, b"dir").await.unwrap().ino;
    let nested = fs.lookup(dir, b"nested").await.unwrap().ino;
    let deep = fs.lookup(nested, b"deep.bin").await.unwrap().ino;
    assert_eq!(read_whole(fs, deep).await, vec![0xab; 12_288]);

    let over = fs.lookup(root, b"override.txt").await.unwrap().ino;
    assert_eq!(read_whole(fs, over).await, b"base version");

    let link = fs.lookup(root, b"link-to-hello").await.unwrap().ino;
    assert_eq!(fs.readlink(link).await.unwrap(), b"hello.txt");

    let pipe = fs.lookup(root, b"pipe").await.unwrap();
    assert_eq!(pipe.mode & S_IFMT, S_IFIFO);
}

async fn verify_golden_version(compressed: &[u8], manifest: &[u8]) {
    let mut decoder = GzDecoder::new(compressed);
    let mut raw = Vec::new();
    decoder.read_to_end(&mut raw).expect("gunzip golden image");
    let image: GoldenImage = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .reject_trailing_bytes()
        .deserialize(&raw)
        .expect("decode golden image");

    let fsck_opts =
        FsckOptions { fail_on_warning: true, do_slow_passes: true, ..Default::default() };

    // 1. Mount read-only, fsck, and read the frozen base filesystem.
    let base_dev = FakeDevice::from_extents(
        image.base_cell,
        image.capacity,
        image.block_size,
        image.base_extents.clone(),
        image.base_root.clone(),
    );
    let base_fs = FxFilesystemBuilder::new()
        .read_only(true)
        .open(DeviceHolder::new(base_dev))
        .await
        .expect("open base image");
    assert!(
        !base_fs.allocator().info().layers.is_empty(),
        "frozen base image must include compacted allocator layers and AllocatorInfo"
    );
    fsck_with_options((*base_fs).clone(), &fsck_opts).await.expect("fsck base");
    let base_root_vol = root_volume((*base_fs).clone()).await.unwrap();
    let base_store = base_root_vol.volume("base").await.unwrap();
    fsck_volume_with_options(&base_fs, &fsck_opts, base_store.store_object_id())
        .await
        .expect("fsck base volume");
    let base_posix = Fxfs::new((*base_fs).clone(), base_store.clone(), true, Tested);
    verify_base_volume(&base_posix, base_store.root_directory_object_id()).await;
    base_posix.stop_delayed().await.unwrap();
    drop((base_posix, base_store, base_root_vol));
    base_fs.close().await.unwrap();

    // 2. Mount read-only, fsck, and read the frozen child filesystem (standalone "main" +
    //    "over-base").
    let base_ro = FakeDevice::from_extents(
        image.base_cell,
        image.capacity,
        image.block_size,
        image.base_extents.clone(),
        image.base_root.clone(),
    );
    let mut child_dev = FakeDevice::from_extents(
        image.child_cell,
        image.capacity,
        image.block_size,
        image.child_extents,
        image.child_root,
    );
    child_dev.attach(DeviceHolder::new(base_ro));
    let child_fs = FxFilesystemBuilder::new()
        .read_only(true)
        .open(DeviceHolder::new(child_dev))
        .await
        .expect("open child image");
    assert!(
        !child_fs.allocator().info().layers.is_empty(),
        "frozen child image must include compacted allocator layers and AllocatorInfo"
    );
    fsck_with_options((*child_fs).clone(), &fsck_opts).await.expect("fsck child");
    let child_root_vol = root_volume((*child_fs).clone()).await.unwrap();

    let main_store = child_root_vol.volume("main").await.unwrap();
    assert!(!main_store.store_info().layers.is_empty(), "main volume must have persistent layers");
    fsck_volume_with_options(&child_fs, &fsck_opts, main_store.store_object_id())
        .await
        .expect("fsck main volume");
    let main_posix = Fxfs::new((*child_fs).clone(), main_store.clone(), true, Tested);
    let main_root = main_store.root_directory_object_id();
    assert_eq!(
        list_dir(&main_posix, main_root).await,
        vec![
            ("data.bin".into(), S_IFREG),
            ("empty".into(), S_IFREG),
            ("journaled.txt".into(), S_IFREG),
            ("sub".into(), S_IFDIR),
        ]
    );
    let journaled = main_posix.lookup(main_root, b"journaled.txt").await.unwrap().ino;
    assert_eq!(read_whole(&main_posix, journaled).await, b"replayed from journal");
    let data = main_posix.lookup(main_root, b"data.bin").await.unwrap().ino;
    assert_eq!(read_whole(&main_posix, data).await, vec![0x5a; 8192]);
    assert_eq!(main_posix.get_xattr(data, b"user.tag").await.unwrap(), b"golden-v66");
    let empty = main_posix.lookup(main_root, b"empty").await.unwrap();
    assert_eq!(empty.size, 0);
    let sub = main_posix.lookup(main_root, b"sub").await.unwrap().ino;
    let leaf = main_posix.lookup(sub, b"leaf").await.unwrap().ino;
    let note = main_posix.lookup(leaf, b"note.txt").await.unwrap();
    let hard = main_posix.lookup(sub, b"hardlink.txt").await.unwrap();
    assert_eq!((note.ino, note.nlink), (hard.ino, 2));
    assert_eq!(read_whole(&main_posix, note.ino).await, b"nested note");
    let rel_link = main_posix.lookup(sub, b"rel-link").await.unwrap().ino;
    assert_eq!(main_posix.readlink(rel_link).await.unwrap(), b"leaf/note.txt");
    let sock = main_posix.lookup(sub, b"sock").await.unwrap();
    assert_eq!(sock.mode & S_IFMT, S_IFSOCK);
    main_posix.stop_delayed().await.unwrap();
    drop((main_posix, main_store));

    let over_store = child_root_vol.volume("over-base").await.unwrap();
    fsck_volume_with_options(&child_fs, &fsck_opts, over_store.store_object_id())
        .await
        .expect("fsck over-base volume");
    let over_posix = Fxfs::new((*child_fs).clone(), over_store.clone(), true, Tested);
    let over_root = over_store.root_directory_object_id();
    assert_eq!(
        list_dir(&over_posix, over_root).await,
        vec![
            ("child-link".into(), S_IFLNK),
            ("child-only.txt".into(), S_IFREG),
            ("dir".into(), S_IFDIR),
            ("hello.txt".into(), S_IFREG),
            ("link-to-hello".into(), S_IFLNK),
            ("override.txt".into(), S_IFREG),
            ("pipe".into(), S_IFIFO),
        ]
    );
    let hello = over_posix.lookup(over_root, b"hello.txt").await.unwrap().ino;
    assert_eq!(read_whole(&over_posix, hello).await, b"hello from base\n");
    assert_eq!(over_posix.get_xattr(hello, b"user.base").await.unwrap(), b"base-xattr");
    let dir = over_posix.lookup(over_root, b"dir").await.unwrap().ino;
    let nested = over_posix.lookup(dir, b"nested").await.unwrap().ino;
    let deep = over_posix.lookup(nested, b"deep.bin").await.unwrap().ino;
    assert_eq!(read_whole(&over_posix, deep).await, vec![0xab; 12_288]);
    let over_file = over_posix.lookup(over_root, b"override.txt").await.unwrap().ino;
    assert_eq!(read_whole(&over_posix, over_file).await, b"child version");
    assert!(over_posix.lookup(over_root, b"remove-me.txt").await.is_err());
    let child_only = over_posix.lookup(over_root, b"child-only.txt").await.unwrap().ino;
    assert_eq!(read_whole(&over_posix, child_only).await, b"only in child");
    assert_eq!(over_posix.get_xattr(child_only, b"user.child").await.unwrap(), b"child-xattr");
    let child_link = over_posix.lookup(over_root, b"child-link").await.unwrap().ino;
    assert_eq!(over_posix.readlink(child_link).await.unwrap(), b"child-only.txt");
    over_posix.stop_delayed().await.unwrap();
    drop((over_posix, over_store, child_root_vol));
    child_fs.close().await.unwrap();

    // 3. Attach the stored golden manifest directly to a fresh filesystem reading through to the
    //    frozen base layers, fsck it, and read back the base volume.
    let base_for_manifest = FakeDevice::from_extents(
        image.base_cell,
        image.capacity,
        image.block_size,
        image.base_extents,
        image.base_root,
    );
    let mut fresh_dev =
        FakeDevice::new(image.capacity / u64::from(image.block_size), image.block_size)
            .calm()
            .with_cell(CHILD_CELL + 1);
    fresh_dev.attach(DeviceHolder::new(base_for_manifest));
    let fresh_fs = FxFilesystem::new_empty(DeviceHolder::new(fresh_dev)).await.unwrap();
    let fresh_root_vol = root_volume((*fresh_fs).clone()).await.unwrap();
    let attached = fresh_root_vol
        .new_volume_over_base("from-manifest", manifest)
        .await
        .expect("new_volume_over_base from golden manifest");
    fsck_with_options((*fresh_fs).clone(), &fsck_opts).await.expect("fsck fresh");
    fsck_volume_with_options(&fresh_fs, &fsck_opts, attached.store_object_id())
        .await
        .expect("fsck fresh over-base");
    let attached_posix = Fxfs::new((*fresh_fs).clone(), attached.clone(), true, Tested);
    verify_base_volume(&attached_posix, attached.root_directory_object_id()).await;
    attached_posix.stop_delayed().await.unwrap();
    drop((attached_posix, attached, fresh_root_vol));
    fresh_fs.close().await.unwrap();
}

#[fuchsia_async::run_singlethreaded(test)]
async fn every_frozen_version_is_read() {
    let write = std::env::var(WRITE_ENV).is_ok_and(|v| v == "1");
    let root = corpus_root();

    // Every directory under formats/fxfs/ must be within FORMAT.versions().
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries {
            let entry = entry.expect("listing formats/fxfs");
            if entry.file_type().expect("entry type").is_dir() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let parsed = name.parse::<u32>();
                assert!(
                    parsed.is_ok_and(|v| FORMAT.versions().contains(&v)),
                    "{}: directory of no version this build reads ({}..={})",
                    entry.path().display(),
                    FORMAT.oldest,
                    FORMAT.current
                );
            }
        }
    }

    // Write current version's corpus files if missing and CLADEFS_FORMATS_WRITE=1 (never overwrite).
    let current_dir = root.join(FORMAT.current.to_string());
    let img_path = current_dir.join("image.bin.gz");
    let man_path = current_dir.join("manifest.bin");
    let fp_path = current_dir.join("fingerprint");
    if write && (!img_path.exists() || !man_path.exists() || !fp_path.exists()) {
        std::fs::create_dir_all(&current_dir).expect("create formats/fxfs/<current>");
        if !img_path.exists() || !man_path.exists() {
            let (compressed, manifest) = build_golden().await;
            if !img_path.exists() {
                std::fs::write(&img_path, &compressed).expect("write image.bin.gz");
            }
            if !man_path.exists() {
                std::fs::write(&man_path, &manifest).expect("write manifest.bin");
            }
        }
        if !fp_path.exists() {
            std::fs::write(&fp_path, render_fingerprints(FORMAT.current))
                .expect("write fingerprint");
        }
    }

    for version in FORMAT.versions() {
        let dir = root.join(version.to_string());
        let expected_fp = render_fingerprints(version);
        let stored_fp = std::fs::read_to_string(dir.join("fingerprint")).unwrap_or_else(|_| {
            panic!(
                "{}: missing; {WRITE_ENV}=1 writes version {version}'s corpus",
                dir.join("fingerprint").display()
            )
        });
        assert_eq!(
            stored_fp, expected_fp,
            "version {version}'s Fxfs type fingerprints changed without a version bump (doc-28)"
        );
        let compressed = std::fs::read(dir.join("image.bin.gz")).unwrap_or_else(|_| {
            panic!(
                "{}: missing; {WRITE_ENV}=1 writes version {version}'s corpus",
                dir.join("image.bin.gz").display()
            )
        });
        let manifest = std::fs::read(dir.join("manifest.bin")).unwrap_or_else(|_| {
            panic!(
                "{}: missing; {WRITE_ENV}=1 writes version {version}'s corpus",
                dir.join("manifest.bin").display()
            )
        });
        verify_golden_version(&compressed, &manifest).await;
    }
}

/// Manifests of versions outside `EARLIEST_SUPPORTED_VERSION..=LATEST_VERSION` are refused with
/// `FxfsError::InvalidVersion` (doc-28, board 10050).
#[fuchsia_async::run_singlethreaded(test)]
async fn manifests_outside_supported_versions_are_refused() {
    let dev = FakeDevice::new(1024, BLOCK_SIZE).calm().with_cell(CHILD_CELL);
    let fs = FxFilesystem::new_empty(DeviceHolder::new(dev)).await.unwrap();
    let root = root_volume((*fs).clone()).await.unwrap();
    for bad_major in [EARLIEST_SUPPORTED_VERSION.major - 1, LATEST_VERSION.major + 1] {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&bad_major.to_le_bytes());
        bytes.extend_from_slice(&0u8.to_le_bytes());
        let err = root
            .new_volume_over_base("bad", &bytes)
            .await
            .err()
            .expect("unsupported manifest version must be refused");
        assert!(
            FxfsError::InvalidVersion.matches(&err),
            "version {bad_major}: expected InvalidVersion, got {err:?}"
        );
    }
    drop(root);
    fs.close().await.unwrap();
}
