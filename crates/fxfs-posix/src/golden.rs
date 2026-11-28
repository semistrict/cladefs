// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: Fxfs's golden corpus (doc-28, board 10050): pins `EARLIEST_SUPPORTED_VERSION` at the
//! frozen version (66), stores a compressed golden image (files, nested directories, hard links,
//! extended attributes, symlinks, special files, and a volume over a shared base's layers, with the
//! base's manifest) together with `get_type_fingerprints`, and verifies every later build mounts,
//! fscks, and reads it back. Shared volumes are removed (board 14527): the image of 66, which they
//! built, stays as it is, its volume over a base now refused where it opens (and reported by fsck),
//! and its manifest still decoding as a store's information; a later version's image needs a
//! builder of its own.

use crate::tests::Tested;
use crate::{Fxfs, ReaddirStart, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFREG, S_IFSOCK};
use bincode::Options as _;
use flate2::read::GzDecoder;
use fprint::TypeFingerprint as _;
use fxfs::errors::FxfsError;
use fxfs::filesystem::FxFilesystemBuilder;
use fxfs::fsck::{FsckOptions, fsck_volume_with_options, fsck_with_options};
use fxfs::object_store::StoreInfo;
use fxfs::object_store::journal::super_block::Root;
use fxfs::object_store::volume::root_volume;
use fxfs::serialized_types::{
    EARLIEST_SUPPORTED_VERSION, LATEST_VERSION, Version, VersionedLatest as _,
    get_type_fingerprints,
};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use storage_device::fake_device::FakeDevice;
use storage_device::{DeviceHolder, ExtentId};

/// cladefs: Fxfs's frozen format range (doc-28, board 10050), declared with `formats::Format` so
/// `scripts/format-freeze.py` guards `EARLIEST_SUPPORTED_VERSION` and `LATEST_VERSION` alongside
/// every other frozen family.
const FORMAT: formats::Format = formats::Format::new("fxfs", 66, 66);

const _: () = {
    assert!(FORMAT.oldest == EARLIEST_SUPPORTED_VERSION.major);
    assert!(FORMAT.current == LATEST_VERSION.major);
    assert!(formats::gates::OLDEST_FXFS == EARLIEST_SUPPORTED_VERSION.major);
    assert!(formats::gates::CURRENT_FXFS == LATEST_VERSION.major);
    assert!(EARLIEST_SUPPORTED_VERSION.minor == 0);
    assert!(LATEST_VERSION.minor == 0);
};

const WRITE_ENV: &str = "CLADEFS_FORMATS_WRITE";

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

    // 2. Mount read-only, fsck, and read the frozen child filesystem's standalone volume "main".
    //    Its volume "over-base", which shared volumes (removed, board 14527) made, is refused
    //    where it opens, so that the volumes open as they are asked for, and fsck reports it.
    let child_dev = FakeDevice::from_extents(
        image.child_cell,
        image.capacity,
        image.block_size,
        image.child_extents,
        image.child_root,
    );
    let child_fs = FxFilesystemBuilder::new()
        .read_only(true)
        .lazy_volumes(true)
        .open(DeviceHolder::new(child_dev))
        .await
        .expect("open child image");
    assert!(
        !child_fs.allocator().info().layers.is_empty(),
        "frozen child image must include compacted allocator layers and AllocatorInfo"
    );
    let issues = Arc::new(Mutex::new(Vec::new()));
    let seen = issues.clone();
    let reporting = FsckOptions {
        on_error: Box::new(move |issue| seen.lock().unwrap().push(format!("{issue:?}"))),
        quiet: true,
        ..Default::default()
    };
    fsck_with_options((*child_fs).clone(), &reporting)
        .await
        .map(drop)
        .expect_err("fsck passed the volume over a base");
    // Taken out of the lock, which no await below may hold.
    let issues = std::mem::take(&mut *issues.lock().unwrap());
    assert!(
        matches!(&issues[..], [issue] if issue.starts_with("Fatal(StoreOverBase(")),
        "{issues:?}"
    );
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

    let error = child_root_vol
        .volume("over-base")
        .await
        .map(drop)
        .expect_err("opened the volume over a base");
    assert!(FxfsError::NotSupported.matches(&error), "{error:#}");
    drop(child_root_vol);
    child_fs.close().await.unwrap();

    // 3. The stored manifest of the base volume, which shared volumes took of it, still decodes as
    //    a store's information: layers of a base, and none of its own.
    let mut reader = manifest;
    let version = Version::deserialize_from(&mut reader).expect("manifest version");
    assert!((EARLIEST_SUPPORTED_VERSION..=LATEST_VERSION).contains(&version), "{version}");
    let info = StoreInfo::deserialize_from_version(&mut reader, version).expect("manifest info");
    assert!(info.layers.is_empty() && !info.base_layers.is_empty() && !info.base_cells.is_empty());
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

    // Write current version's fingerprint if missing and CLADEFS_FORMATS_WRITE=1 (never overwrite).
    let current_dir = root.join(FORMAT.current.to_string());
    let fp_path = current_dir.join("fingerprint");
    if write && !fp_path.exists() {
        std::fs::create_dir_all(&current_dir).expect("create formats/fxfs/<current>");
        std::fs::write(&fp_path, render_fingerprints(FORMAT.current)).expect("write fingerprint");
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
        let compressed = std::fs::read(dir.join("image.bin.gz"))
            .unwrap_or_else(|_| panic!("{}: missing", dir.join("image.bin.gz").display()));
        let manifest = std::fs::read(dir.join("manifest.bin"))
            .unwrap_or_else(|_| panic!("{}: missing", dir.join("manifest.bin").display()));
        verify_golden_version(&compressed, &manifest).await;
    }
}
