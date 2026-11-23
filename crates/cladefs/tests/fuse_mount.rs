// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Tests of cells mounted with FUSE through Linux's kernel (`cladefs::fuse::MountOptions`,
//! `cladefs-mount`, TASK-222; cladefsd's mount of cells, TASK-475.2), used with ordinary file
//! system calls, then read back from the cell.
//!
//! A test run by root mounts with mount(2); any other user's, through `fusermount3`, which the
//! hosts the tests run on have (libfuse 3's `fuse3`: .github/workflows/ci.yml,
//! scripts/lib/linux-host-setup.sh, scripts/lima-setup.sh).
//!
//! Each test unmounts lazily, as the daemon and `cladefs-mount` do, and joins the mount, which
//! waits until the kernel is done with it: other processes of the host may look into a new mount
//! at any time, as a desktop session's trash monitor does every one (`gvfsd-trash`: the root's
//! access, an inotify watch, `.Trash` and `.Trash-1000` looked up), and an unmount that comes
//! during one of their calls fails, busy (TASK-705).

#![cfg(target_os = "linux")]

use cladefs::fuse::MountOptions;
use cladefs::{Filesystem, SetAttr, Storage};
use futures::executor::block_on;
use std::ffi::CString;
use std::io::{BufRead as _, ErrorKind, Read as _, Seek as _, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

const CELL_SIZE: u64 = 256 << 20;

/// A new cell at `path`, formatted, its root directory the test's user's, as a cell made for
/// them would be: mounted by them, the kernel checks their permissions.
fn format(path: &Path) -> Filesystem {
    let storage = local_cell::upload::wait_on(Storage::create(path, CELL_SIZE)).unwrap();
    let fs = block_on(Filesystem::format(storage)).unwrap();
    // SAFETY: Plain calls returning the process's IDs.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let owner = SetAttr { uid: Some(uid), gid: Some(gid), ..Default::default() };
    block_on(fs.root().setattr(&owner)).unwrap();
    fs
}

/// The cell directory and mount point of a test, in `dir`: a `.gvfs`, which GIO takes for its own
/// and leaves alone (GLib's `g_unix_is_mount_path_system_internal`), as a desktop session's trash
/// monitor (`gvfsd-trash`) looks into every other new mount and stats its root as it changes,
/// which Linux's FUSE client answers the test's own stats from its cache half updated by
/// (TASK-306, TASK-706).
fn paths(dir: &Path) -> (PathBuf, PathBuf) {
    let mountpoint = dir.join(".gvfs");
    std::fs::create_dir(&mountpoint).unwrap();
    (dir.join("store"), mountpoint)
}

/// The names in the directory `path`, sorted.
fn names(path: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn cstring(path: &Path) -> CString {
    CString::new(path.as_os_str().as_bytes()).unwrap()
}

/// The contents of `name`, a file in `dir`, read through the cell's filesystem.
async fn contents(dir: &cladefs::Node, name: &[u8]) -> Vec<u8> {
    let (file, attr) = dir.lookup(name).await.unwrap();
    let mut contents = vec![0; attr.size as usize];
    assert_eq!(file.read(0, &mut contents).await, Ok(contents.len()));
    contents
}

/// A pattern of `len` bytes that no shift of it repeats within a page.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// A cell mounted through the kernel is used as any filesystem is: directories, files read and
/// written, large ones too, permissions, symlinks, hard links, renames, listings, a file unlinked
/// while open, truncations, extended attributes, statvfs, FIFOs, exclusive creates, fsync. Once
/// unmounted and closed, the cell is whole, and holds what was done.
#[test]
fn a_cell_mounted_through_the_kernel_is_used_as_any_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let (cell_dir, m) = paths(dir.path());
    let mount = MountOptions::new().mount(format(&cell_dir), &m).expect("mount failed");
    assert_eq!(mount.mountpoint(), m);

    std::fs::create_dir_all(m.join("a/b")).unwrap();
    std::fs::write(m.join("a/b/file"), "hello").unwrap();
    assert_eq!(std::fs::read_to_string(m.join("a/b/file")).unwrap(), "hello");
    let metadata = std::fs::metadata(m.join("a/b/file")).unwrap();
    assert_eq!((metadata.len(), metadata.is_file()), (5, true));
    // SAFETY: Plain calls returning the process's IDs.
    assert_eq!((metadata.uid(), metadata.gid()), unsafe { (libc::getuid(), libc::getgid()) });

    std::fs::set_permissions(m.join("a/b/file"), std::fs::Permissions::from_mode(0o640)).unwrap();
    assert_eq!(std::fs::metadata(m.join("a/b/file")).unwrap().mode() & 0o7777, 0o640);

    std::os::unix::fs::symlink("b/file", m.join("a/link")).unwrap();
    assert_eq!(std::fs::read_link(m.join("a/link")).unwrap(), Path::new("b/file"));
    assert_eq!(std::fs::read_to_string(m.join("a/link")).unwrap(), "hello");
    std::fs::hard_link(m.join("a/b/file"), m.join("a/hard")).unwrap();
    assert_eq!(std::fs::metadata(m.join("a/b/file")).unwrap().nlink(), 2);
    std::fs::rename(m.join("a/hard"), m.join("renamed")).unwrap();
    assert_eq!(names(&m.join("a")), ["b", "link"]);
    assert_eq!(names(&m), ["a", "renamed"]);

    // Larger than a write or read the kernel sends at once.
    let large = pattern((3 << 20) + 123);
    std::fs::write(m.join("large"), &large).unwrap();
    assert_eq!(std::fs::read(m.join("large")).unwrap(), large);

    // A file unlinked while open is read and written through the open file.
    let mut open = std::fs::File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(m.join("temporary"))
        .unwrap();
    open.write_all(b"temporary").unwrap();
    std::fs::remove_file(m.join("temporary")).unwrap();
    assert_eq!(std::fs::metadata(m.join("temporary")).unwrap_err().kind(), ErrorKind::NotFound);
    open.rewind().unwrap();
    let mut read = String::new();
    open.read_to_string(&mut read).unwrap();
    assert_eq!((read.as_str(), open.metadata().unwrap().nlink()), ("temporary", 0));
    drop(open);

    // Truncations: by an open, and by a length set.
    std::fs::write(m.join("truncated"), "contents").unwrap();
    std::fs::File::create(m.join("truncated")).unwrap();
    assert_eq!(std::fs::metadata(m.join("truncated")).unwrap().len(), 0);
    let file = std::fs::File::options().write(true).open(m.join("truncated")).unwrap();
    file.set_len(10).unwrap();
    file.sync_all().unwrap();
    drop(file);
    assert_eq!(std::fs::read(m.join("truncated")).unwrap(), [0; 10]);

    // Extended attributes.
    let renamed = cstring(&m.join("renamed"));
    // SAFETY: setxattr(2) and getxattr(2), with NUL-terminated strings and buffers that outlive
    // the calls.
    unsafe {
        let value = b"attribute";
        let set = libc::setxattr(
            renamed.as_ptr(),
            c"user.cladefs".as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        );
        assert_eq!(set, 0, "setxattr: {}", std::io::Error::last_os_error());
        let mut got = [0u8; 64];
        let len = libc::getxattr(
            renamed.as_ptr(),
            c"user.cladefs".as_ptr(),
            got.as_mut_ptr().cast(),
            got.len(),
        );
        assert_eq!(len, value.len() as isize, "getxattr: {}", std::io::Error::last_os_error());
        assert_eq!(&got[..value.len()], value);
    }

    // SAFETY: statvfs(2) into a struct this test owns, of a NUL-terminated path.
    let statvfs = unsafe {
        let mut statvfs: libc::statvfs = std::mem::zeroed();
        assert_eq!(libc::statvfs(cstring(&m).as_ptr(), &mut statvfs), 0);
        statvfs
    };
    assert_eq!((statvfs.f_bsize, statvfs.f_blocks), (4096, CELL_SIZE / 4096));
    assert!(statvfs.f_bfree < statvfs.f_blocks, "{} blocks free", statvfs.f_bfree);

    // SAFETY: mkfifo(3), with a NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(cstring(&m.join("fifo")).as_ptr(), 0o644) }, 0);
    assert!(std::fs::metadata(m.join("fifo")).unwrap().file_type().is_fifo());

    let exclusive = std::fs::File::options().write(true).create_new(true).open(m.join("renamed"));
    assert_eq!(exclusive.unwrap_err().kind(), ErrorKind::AlreadyExists);
    assert_eq!(std::fs::remove_dir(m.join("a")).unwrap_err().kind(), ErrorKind::DirectoryNotEmpty);
    std::fs::File::open(m.join("renamed")).unwrap().sync_all().unwrap();

    mount.unmount(true).expect("unmount failed");
    let storage = mount.join().expect("closing failed");
    let storage = block_on(storage.check()).expect("the cell is not whole");

    block_on(async {
        let fs = Filesystem::mount(storage).await.unwrap();
        let root = fs.root();
        let (a, _) = root.lookup(b"a").await.unwrap();
        let (b, _) = a.lookup(b"b").await.unwrap();
        assert_eq!(contents(&b, b"file").await, b"hello");
        assert_eq!(contents(root, b"renamed").await, b"hello");
        assert_eq!(contents(root, b"large").await, large);
        assert_eq!(contents(root, b"truncated").await, [0; 10]);
        let (renamed, attr) = root.lookup(b"renamed").await.unwrap();
        assert_eq!((attr.nlink, attr.perm), (2, 0o640));
        assert_eq!(renamed.getxattr(b"user.cladefs").await.unwrap(), b"attribute");
        let (link, _) = a.lookup(b"link").await.unwrap();
        assert_eq!(link.readlink().await.unwrap(), b"b/file");
        let (_, fifo) = root.lookup(b"fifo").await.unwrap();
        assert_eq!(fifo.file_type, cladefs::FileType::Fifo);
        assert_eq!(root.lookup(b"temporary").await.unwrap_err(), cladefs::Errno::ENOENT);
        drop((a, b, renamed, link));
        fs.close().await.unwrap();
    });
}

/// A mount dropped before it is joined is unmounted, lazily, and closed: its cell is whole, holds
/// what was written, and may be opened again.
#[test]
fn a_mount_dropped_unmounts_and_closes() {
    let dir = tempfile::tempdir().unwrap();
    let (cell_dir, m) = paths(dir.path());
    let mount = MountOptions::new().mount(format(&cell_dir), &m).unwrap();
    std::fs::write(m.join("file"), "written").unwrap();
    drop(mount);
    assert_eq!(names(&m), Vec::<String>::new(), "still mounted");
    block_on(async {
        let storage =
            local_cell::upload::wait_on(Storage::open(&cell_dir)).unwrap().check().await.unwrap();
        let fs = Filesystem::mount(storage).await.unwrap();
        assert_eq!(contents(fs.root(), b"file").await, b"written");
        fs.close().await.unwrap();
    });
}

/// TASK-411.7 (TASK-411's AC #3, FUSE mounts; Linux): an encrypted cell, its tenant's KEK
/// in the host's KMS (the default tenant's, whose cells `Storage` makes), mounted through the
/// kernel: a file written is read back through the mount and, unmounted and closed, from the cell
/// opened again; no file of the cell directory, nor of its bucket's, holds it in the clear.
#[test]
fn an_encrypted_cell_mounted_holds_nothing_in_the_clear() {
    let keys = tempfile::tempdir().unwrap();
    let kms = extent_keys::kms::LocalKms::open(keys.path().join("kms")).unwrap();
    kms.create(0).unwrap();
    cladefs::bucket::set_kms(std::sync::Arc::new(kms)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (cell_dir, m) = paths(dir.path());
    // Data that compresses not: from a xorshift generator.
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let data: Vec<u8> = (0..(1 << 20) + 123)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect();
    let mount = MountOptions::new().mount(format(&cell_dir), &m).unwrap();
    std::fs::write(m.join("file"), &data).unwrap();
    assert!(std::fs::read(m.join("file")).unwrap() == data, "read back otherwise");
    drop(mount);
    block_on(async {
        let storage =
            local_cell::upload::wait_on(Storage::open(&cell_dir)).unwrap().check().await.unwrap();
        let fs = Filesystem::mount(storage).await.unwrap();
        assert!(contents(fs.root(), b"file").await == data, "reopened, read otherwise");
        fs.close().await.unwrap();
    });
    assert_eq!(cladefs::testing::files_holding(dir.path(), &[&data[..64]]), Vec::<String>::new());
}

/// A child process, killed if the test ends while it runs.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `cladefs-mount [FLAGS] CELL_DIR MOUNTPOINT`, its output piped, once it has said it is mounted.
fn cladefs_mount(flags: &[&str], cell_dir: &Path, mountpoint: &Path) -> KillOnDrop {
    let mut child = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_cladefs-mount"))
            .args(flags)
            .arg(cell_dir)
            .arg(mountpoint)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut said = String::new();
    std::io::BufReader::new(child.0.stdout.take().unwrap()).read_line(&mut said).unwrap();
    if said.is_empty() {
        let mut stderr = String::new();
        child.0.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
        panic!("cladefs-mount ended unmounted, {}: {stderr}", child.0.wait().unwrap());
    }
    assert_eq!(said.trim_end(), mountpoint.to_str().unwrap());
    child
}

/// `cladefs-mount` serves a cell until SIGTERM, which unmounts it; then it closes it and exits 0,
/// and the cell holds what was written through the mount.
#[test]
fn cladefs_mount_serves_a_cell_until_sigterm() {
    let dir = tempfile::tempdir().unwrap();
    let (cell_dir, m) = paths(dir.path());
    block_on(format(&cell_dir).close()).unwrap();
    let child = cladefs_mount(&[], &cell_dir, &m);
    std::fs::create_dir(m.join("dir")).unwrap();
    std::fs::write(m.join("dir/file"), "through cladefs-mount").unwrap();
    terminate(child, &m);
    block_on(async {
        let storage =
            local_cell::upload::wait_on(Storage::open(&cell_dir)).unwrap().check().await.unwrap();
        let fs = Filesystem::mount(storage).await.unwrap();
        let (dir, _) = fs.root().lookup(b"dir").await.unwrap();
        assert_eq!(contents(&dir, b"file").await, b"through cladefs-mount");
        drop(dir);
        fs.close().await.unwrap();
    });
}

/// Sends `cladefs-mount`'s `child` SIGTERM, and checks that it unmounted `mountpoint` and exited 0.
fn terminate(mut child: KillOnDrop, mountpoint: &Path) {
    // SAFETY: Signals a process this test started.
    assert_eq!(unsafe { libc::kill(child.0.id() as i32, libc::SIGTERM) }, 0);
    let status = child.0.wait().unwrap();
    let mut stderr = String::new();
    child.0.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
    assert!(status.success(), "{status}: {stderr}");
    assert_eq!(names(mountpoint), Vec::<String>::new(), "still mounted");
}

/// `cladefs-mount --new CAPACITY` makes a new cell of CAPACITY bytes, its filesystem empty, its
/// root a new volume's (0755, root's), and mounts it; a cell already there it refuses, as it does
/// a capacity that isn't a number.
#[test]
fn cladefs_mount_makes_a_new_cell() {
    let dir = tempfile::tempdir().unwrap();
    let (cell_dir, m) = paths(dir.path());
    let child = cladefs_mount(&["--new", "67108864"], &cell_dir, &m);
    assert_eq!(names(&m), Vec::<String>::new());
    let root = std::fs::metadata(&m).unwrap();
    assert_eq!((root.mode() & 0o7777, root.uid(), root.gid()), (0o755, 0, 0));
    terminate(child, &m);
    block_on(async {
        let storage =
            local_cell::upload::wait_on(Storage::open(&cell_dir)).unwrap().check().await.unwrap();
        let fs = Filesystem::mount(storage).await.unwrap();
        assert_eq!(fs.statfs().await.unwrap().blocks, 67108864 / 4096);
        fs.close().await.unwrap();
    });

    for (args, said) in [
        (vec!["--new", "67108864"], "making"),
        (vec!["--new", "lots"], "--new CAPACITY, in bytes?"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_cladefs-mount"))
            .args(args)
            .arg(&cell_dir)
            .arg(&m)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "mounted");
        assert!(stderr.contains(said), "{stderr}");
    }
}

/// A cell mounted is its mount's alone: `cladefs-mount` fails to open it, once it has waited for it
/// as long as the extent store waits for another process's (`TAKE_WAIT`), exits unsuccessfully,
/// and says why.
#[test]
fn a_cell_mounted_is_not_mounted_again() {
    let dir = tempfile::tempdir().unwrap();
    let (cell_dir, m) = paths(dir.path());
    let mount = MountOptions::new().mount(format(&cell_dir), &m).unwrap();
    let other = dir.path().join("other");
    std::fs::create_dir(&other).unwrap();
    let output =
        Command::new(env!("CARGO_BIN_EXE_cladefs-mount")).arg(&cell_dir).arg(&other).output();
    let output = output.unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "mounted twice");
    assert!(stderr.contains(&format!("opening {}", cell_dir.display())), "{stderr}");
    assert!(stderr.contains("another process has the store's cells open"), "{stderr}");
    assert_eq!(names(&other), Vec::<String>::new());
    mount.unmount(true).unwrap();
    block_on(mount.join().unwrap().check()).unwrap();
}

/// TASK-306: a new cell's root, given the user for its owner and mounted, reads 0755 through the
/// kernel, the user's, as it did before it was mounted: many mounted at once, each by a thread of
/// its own.
#[test]
fn new_roots_read_their_permissions_through_the_kernel() {
    // SAFETY: Plain calls returning the process's IDs.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    std::thread::scope(|scope| {
        for thread in 0..8 {
            scope.spawn(move || {
                for round in 0..25 {
                    let dir = tempfile::tempdir().unwrap();
                    let (cell_dir, m) = paths(dir.path());
                    let mount = MountOptions::new().mount(format(&cell_dir), &m).unwrap();
                    let meta = std::fs::metadata(&m).unwrap();
                    let got = (meta.mode() & 0o7777, meta.uid(), meta.gid(), meta.nlink());
                    assert_eq!(got, (0o755, uid, gid, 2), "thread {thread}, round {round}");
                    mount.unmount(true).unwrap();
                    block_on(mount.join().unwrap().check()).unwrap();
                }
            });
        }
    });
}

/// TASK-495: the times a change gives a node are the ones the kernel shows next, though others stat
/// it all the while. The kernel marks a directory's attributes stale as its entries change, but
/// takes a stat's reply sent before the change as fresh, and keeps it as long as the mount says
/// (`fuse_dir_changed`, which leaves the inode's `attr_version` alone): unless the server tells the
/// kernel the node changed (`FUSE_NOTIFY_INVAL_INODE`), a stat crossing a mkdir leaves the
/// directory's old times for the day the mount keeps them. As a renamed node's ctime is.
/// Others list and stat the nodes as `ls -l` does.
#[test]
fn changes_show_their_times_though_others_stat_the_nodes_meanwhile() {
    let dir = tempfile::tempdir().unwrap();
    let (cell_dir, m) = paths(dir.path());
    let mount = MountOptions::new().mount(format(&cell_dir), &m).unwrap();
    let (a, b) = (m.join("a"), m.join("b"));
    std::fs::create_dir(&a).unwrap();
    std::fs::create_dir(&b).unwrap();
    type Times = ((i64, i64), (i64, i64));
    let times = |path: &Path| -> Times {
        let meta = std::fs::symlink_metadata(path).unwrap();
        ((meta.mtime(), meta.mtime_nsec()), (meta.ctime(), meta.ctime_nsec()))
    };
    let done = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        // Threads spinning beside them, more than the CPUs, so that the server's threads are
        // put off between reading a node's attributes and replying with them, as under load.
        let cpus = std::thread::available_parallelism().map_or(4, |cpus| cpus.get());
        for _ in 0..cpus {
            scope.spawn(|| {
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
            });
        }
        for _ in 0..8 {
            scope.spawn(|| {
                // Each directory listed, then statted: the listing marks its atime stale, so
                // that the stat asks the server, as `ls -l`'s do.
                let list_and_stat = |path: &Path| {
                    let listed = std::fs::read_dir(path).into_iter().flatten().flatten();
                    let _ = (listed.count(), std::fs::symlink_metadata(path));
                };
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    list_and_stat(&m);
                    for dir in [&a, &b] {
                        list_and_stat(dir);
                        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                            list_and_stat(&entry.path());
                        }
                    }
                }
            });
        }
        // Whether both a node's mtime and its ctime changed.
        let both = |before: Times, after: Times| after.0 != before.0 && after.1 != before.1;
        let mut stale = Vec::new();
        let began = std::time::Instant::now();
        let mut round = 0;
        while began.elapsed() < std::time::Duration::from_secs(20) {
            round += 1;
            let (made, moved) = (a.join(format!("{round}")), b.join(format!("{round}")));
            let before = times(&a);
            std::fs::create_dir(&made).unwrap();
            if !both(before, times(&a)) {
                stale.push(format!("{round}: mkdir left a's times {before:?}"));
            }
            let before = [times(&a), times(&b), times(&made)];
            std::fs::rename(&made, &moved).unwrap();
            let after = [times(&a), times(&b), times(&moved)];
            if !both(before[0], after[0]) || !both(before[1], after[1]) {
                stale.push(format!("{round}: rename left a's or b's times {before:?}"));
            }
            if after[2].1 == before[2].1 {
                stale.push(format!("{round}: rename left the moved node's ctime {:?}", before[2]));
            }
            // The mount's root, which the server numbers apart: its status changed, then its
            // entries.
            let before = times(&m);
            let perm = std::fs::Permissions::from_mode(if round % 2 == 0 { 0o750 } else { 0o755 });
            std::fs::set_permissions(&m, perm).unwrap();
            let after = times(&m);
            if after.1 == before.1 {
                stale.push(format!("{round}: chmod left the root's ctime {before:?}"));
            }
            let before = after;
            std::os::unix::fs::symlink("x", m.join(format!("s{round}"))).unwrap();
            if !both(before, times(&m)) {
                stale.push(format!("{round}: symlink left the root's times {before:?}"));
            }
            // A file made in the root, then a directory: TASK-706's cases.
            let before = times(&m);
            drop(std::fs::File::create(m.join(format!("f{round}"))).unwrap());
            let made = times(&m);
            if !both(before, made) {
                stale.push(format!("{round}: create left the root's times {before:?}, {made:?}"));
            }
            std::fs::create_dir(m.join(format!("d{round}"))).unwrap();
            let after = times(&m);
            if !both(made, after) {
                stale.push(format!("{round}: mkdir left the root's times {made:?}, {after:?}"));
            }
        }
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(stale, Vec::<String>::new(), "of {round} rounds");
    });
    mount.unmount(true).unwrap();
    block_on(mount.join().unwrap().check()).unwrap();
}

/// cladefsd serving at `vm`, its VM socket, of placement `token`, its bucket the directory
/// `bucket`, mounting cells at `dir`/mnt, their directories in `dir`/cells, its control socket
/// `dir`/control (TASK-475.2), once its control socket is there, which it makes once its mount is:
/// its child, its control socket and its mount point.
fn cladefsd_mounting(dir: &Path, bucket: &Path, token: u64) -> (KillOnDrop, PathBuf, PathBuf) {
    let (socket, cells, mountpoint) = (dir.join("control"), dir.join("cells"), dir.join("mnt"));
    std::fs::create_dir_all(bucket).unwrap();
    std::fs::create_dir_all(&mountpoint).unwrap();
    let mut command = cladefsd(&dir.join("vm.sock"), bucket, token);
    command
        .env("CLADEFSD_GRPC", &socket)
        .env("CLADEFSD_MOUNT", &mountpoint)
        .env("CLADEFSD_MOUNT_STORES", &cells);
    (serving(command, &socket), socket, mountpoint)
}

/// cladefsd serving at `vm`, of placement `token`, its bucket the directory `bucket`, its memory
/// fixed, as a test's, and its standard error piped.
fn cladefsd(vm: &Path, bucket: &Path, token: u64) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cladefsd"));
    command
        .arg(vm)
        .arg((256u64 << 20).to_string())
        .env("CLADEFSD_BUCKET", url::Url::from_directory_path(bucket).unwrap().to_string())
        .env("CLADEFSD_TOKEN", token.to_string())
        .stderr(Stdio::piped());
    command
}

/// `command`, a cladefsd's, started, once `socket` is there: or what it said, if it ended first.
fn serving(mut command: Command, socket: &Path) -> KillOnDrop {
    let mut child = KillOnDrop(command.spawn().unwrap());
    while !socket.exists() {
        if let Some(status) = child.0.try_wait().unwrap() {
            let mut stderr = String::new();
            child.0.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
            panic!("cladefsd ended, {status}: {stderr}");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    child
}

/// `cladefsctl mount ARGS` through the daemon whose control socket is `socket`.
fn cladefsctl_mount_command(socket: &str, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cladefsctl"));
    command.env("CLADEFSD_GRPC", socket).arg("mount").args(args);
    command
}

/// `cladefsctl mount ARGS` through the daemon whose control socket is `socket`: what it prints,
/// once it succeeds.
fn cladefsctl_mount(socket: &str, args: &[&str]) -> String {
    let output = cladefsctl_mount_command(socket, args).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "cladefsctl mount {args:?}: {}: {stderr}", output.status);
    String::from_utf8(output.stdout).unwrap()
}

/// One mount of many cells (TASK-302), of tenants' cells (TASK-364), cladefsd's (TASK-475.2): a
/// cell has one kind (doc-20), made or attached through its control socket and shown at TENANT/CELL
/// (nothing renamed between cells, nothing made in the mount's directories of its own); its cell's
/// objects in the bucket under its tenant's prefix. A cell detached goes, written back, so that it
/// attaches again from its directory, or, that gone, from the bucket; a cell with a file open is
/// detached once the file is closed.
#[test]
fn one_mount_attaches_and_detaches_many_cells() {
    let dir = tempfile::tempdir().unwrap();
    let (child, socket, m) = cladefsd_mounting(dir.path(), &dir.path().join("bucket"), 1);
    let socket = socket.to_str().unwrap();
    assert_eq!(names(&m), Vec::<String>::new());
    let capacity = CELL_SIZE.to_string();
    let (one, two) = ("0000000000001", "0000000000002");
    let lines = |args: &[&str]| -> Vec<String> {
        cladefsctl_mount(socket, args).lines().map(str::to_owned).collect()
    };
    // Three new cells, two of tenant 1's, one of tenant 2's.
    let create = |tenant: &str| match &lines(&["create", tenant, &capacity])[..] {
        [cell] => cell.clone(),
        printed => panic!("created {printed:?}"),
    };
    let (a, c, b) = (create("1"), create("1"), create("2"));
    assert_eq!(names(&m), [one, two]);
    let mut cells = vec![a.clone(), c.clone()];
    cells.sort();
    assert_eq!(names(&m.join(one)), cells);
    let (at_a, at_c, at_b) = (m.join(one).join(&a), m.join(one).join(&c), m.join(two).join(&b));
    std::fs::write(at_a.join("file"), "in a").unwrap();
    std::fs::write(at_c.join("file"), "in c").unwrap();
    std::fs::write(at_b.join("file"), "in b").unwrap();
    assert_eq!(std::fs::read_to_string(at_a.join("file")).unwrap(), "in a");
    assert_eq!(std::fs::read_to_string(at_b.join("file")).unwrap(), "in b");
    for (from, to) in
        [(at_a.join("file"), at_c.join("moved")), (at_a.join("file"), at_b.join("moved"))]
    {
        assert_eq!(std::fs::rename(&from, &to).unwrap_err().raw_os_error(), Some(libc::EXDEV));
    }
    for made in [m.join("c"), m.join(one).join("c")] {
        let refused = std::fs::create_dir(&made).unwrap_err();
        assert_eq!(refused.raw_os_error(), Some(libc::EPERM), "{}", made.display());
    }
    assert_ne!(
        std::fs::metadata(at_a.join("file")).unwrap().ino(),
        std::fs::metadata(at_c.join("file")).unwrap().ino()
    );
    let mut listed = vec![format!("{one} {a}"), format!("{one} {c}")];
    listed.sort();
    listed.push(format!("{two} {b}"));
    assert_eq!(cladefsctl_mount(socket, &["list"]), listed.join("\n") + "\n");
    // The cells' objects are under their tenants' prefixes: each cell's head.
    let bucket = dir.path().join("bucket");
    let cell_a = bucket.join(format!("tenants/{one}/cells/{a}"));
    assert!(cell_a.join("head").exists(), "no head at {}", cell_a.display());
    assert!(bucket.join(format!("tenants/{two}/cells/{b}/head")).exists());

    // Detached, and attached again from its directory; another cell detached, its directory gone,
    // then attached again from the bucket.
    cladefsctl_mount(socket, &["detach", "1", &a]);
    assert_eq!(names(&m.join(one)), [c.as_str()]);
    assert_eq!(lines(&["attach", "1", &a]), [a.clone()]);
    assert_eq!(std::fs::read_to_string(at_a.join("file")).unwrap(), "in a");
    cladefsctl_mount(socket, &["detach", "2", &b]);
    std::fs::remove_dir_all(dir.path().join("cells").join(two)).unwrap();
    assert_eq!(lines(&["attach", "2", &b]), [b.clone()]);
    assert_eq!(std::fs::read_to_string(at_b.join("file")).unwrap(), "in b");

    // A file open holds its cell's detach until it is closed.
    let open = std::fs::File::open(at_c.join("file")).unwrap();
    let mut detach = KillOnDrop(
        cladefsctl_mount_command(socket, &["detach", "1", &c])
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let detaching =
        |listed: &[String]| -> String { listed.iter().map(|line| line.clone() + "\n").collect() };
    let mut with_c = listed.clone();
    for line in &mut with_c {
        if *line == format!("{one} {c}") {
            line.push_str(" detaching");
        }
    }
    while cladefsctl_mount(socket, &["list"]) != detaching(&with_c) {}
    assert!(detach.0.try_wait().unwrap().is_none(), "detached with a file open");
    drop(open);
    assert!(detach.0.wait().unwrap().success());
    let without_c: Vec<String> =
        listed.iter().filter(|line| **line != format!("{one} {c}")).cloned().collect();
    assert_eq!(cladefsctl_mount(socket, &["list"]), detaching(&without_c));

    terminate(child, &m);
    // Each cell is whole, and holds what was written.
    block_on(async {
        let cells = dir.path().join("cells");
        for (cell_dir, file) in [
            (cells.join(one).join(&a), "in a"),
            (cells.join(one).join(&c), "in c"),
            (cells.join(two).join(&b), "in b"),
        ] {
            let storage = local_cell::upload::wait_on(Storage::open(&cell_dir))
                .unwrap()
                .check()
                .await
                .unwrap();
            let fs = Filesystem::mount(storage).await.unwrap();
            assert_eq!(contents(fs.root(), b"file").await, file.as_bytes());
            fs.close().await.unwrap();
        }
    });
}

/// SIGTERM drains cladefsd's VMs' cells and its mount of cells alike, within one bound
/// (TASK-475.2): a VM process's cell closed, its last commit durable; the mount gone, its
/// cell's write written back and uploaded, so that a later placement's daemon, with none of the
/// cells, attaches the cell from the bucket and reads it; the daemon exits 0.
#[tokio::test(flavor = "multi_thread")]
async fn sigterm_drains_vms_cells_and_the_mount_of_cells() {
    let dir = tempfile::tempdir().unwrap();
    let bucket = dir.path().join("bucket");
    let first = dir.path().join("first");
    std::fs::create_dir(&first).unwrap();
    let (child, socket, m) = cladefsd_mounting(&first, &bucket, 1);
    let socket = socket.to_str().unwrap();
    let capacity = CELL_SIZE.to_string();
    let created = cladefsctl_mount(socket, &["create", "1", &capacity]);
    let cell = created.lines().next().unwrap().to_owned();
    let shown = m.join("0000000000001").join(&cell);
    std::fs::write(shown.join("file"), "drained").unwrap();

    let vm = cladefs::CellClient::connect(first.join("vm.sock"), first.join("vm"), 1 << 28);
    let vm = vm.unwrap();
    vm.commit_ordered(Some(b"vm drained")).unwrap();
    terminate(child, &m);
    drop(vm);
    assert_eq!(
        cladefs::OpenCell::open(first.join("vm")).await.unwrap().root().unwrap(),
        b"vm drained"
    );

    let second = dir.path().join("second");
    std::fs::create_dir(&second).unwrap();
    let (child, socket, m) = cladefsd_mounting(&second, &bucket, 2);
    let socket = socket.to_str().unwrap();
    assert_eq!(cladefsctl_mount(socket, &["attach", "1", &cell]), format!("{cell}\n"));
    let shown = m.join("0000000000001").join(&cell);
    assert_eq!(std::fs::read_to_string(shown.join("file")).unwrap(), "drained");
    terminate(child, &m);
}

/// A daemon mounting no cells has no Mount service: `cladefsctl mount` fails, saying so
/// (TASK-475.2).
#[test]
fn a_daemon_mounting_no_cells_serves_no_mount_calls() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("control");
    let mut command = cladefsd(&dir.path().join("vm.sock"), &dir.path().join("bucket"), 1);
    std::fs::create_dir(dir.path().join("bucket")).unwrap();
    command.env("CLADEFSD_GRPC", &socket);
    let _daemon = serving(command, &socket);
    let output = cladefsctl_mount_command(socket.to_str().unwrap(), &["list"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "listed");
    assert!(
        stderr.contains("listing the cells: code: 'Operation is not implemented or not supported'"),
        "{stderr}"
    );
}

/// A cell cladefsd's mount has attached is the mount's alone (TASK-475.3): a VM process opening
/// its cell, and the control API forking it, each fail with `EBUSY` at once, not after waiting
/// for it to be let go; detached, its cell forks.
#[test]
fn a_cell_mounted_is_the_mounts_alone() {
    let dir = tempfile::tempdir().unwrap();
    let (child, socket, m) = cladefsd_mounting(dir.path(), &dir.path().join("bucket"), 1);
    let socket = socket.to_str().unwrap();
    let created = cladefsctl_mount(socket, &["create", "1", &CELL_SIZE.to_string()]);
    let cell = created.lines().next().unwrap().to_owned();
    let cell_dir = dir.path().join("cells").join("0000000000001").join(&cell);

    let started = std::time::Instant::now();
    let vm = cladefs::CellClient::connect(dir.path().join("vm.sock"), &cell_dir, CELL_SIZE);
    assert_eq!(vm.err().and_then(|err| err.errno()), Some(cladefs::Errno::EBUSY));
    let control = cladefs::cladefsd::control::Client::connect(socket).unwrap();
    let forked = control.fork_cell(&cell_dir, &dir.path().join("fork"));
    assert_eq!(forked.err().and_then(|err| err.errno()), Some(cladefs::Errno::EBUSY));
    let waited = started.elapsed();
    assert!(waited < std::time::Duration::from_secs(5), "refused after {waited:?}");

    cladefsctl_mount(socket, &["detach", "1", &cell]);
    control.fork_cell(&cell_dir, &dir.path().join("fork")).unwrap();
    terminate(child, &m);
}

/// Board 8539: a handler that panics is answered `EIO`, and its worker serves on: nine lookups
/// that panic, more than the mount's eight workers, and the mount still answers. Until then a
/// panic ended its worker for good, the request it read never answered, and once eight had, every
/// request went unread, their callers hung in the kernel for the server's life.
#[test]
fn handlers_that_panic_are_answered_eio_and_their_workers_serve_on() {
    let dir = tempfile::tempdir().unwrap();
    let (cell_dir, m) = paths(dir.path());
    let mount = MountOptions::new().mount(format(&cell_dir), &m).unwrap();
    fail::cfg("fuse::lookup", "9*panic").unwrap();
    let (sent, answered) = std::sync::mpsc::channel();
    let looking = m.clone();
    let looker = std::thread::spawn(move || {
        let errno = |name: &str| std::fs::metadata(looking.join(name)).unwrap_err().raw_os_error();
        let panicked: Vec<_> = (0..9).map(|n| errno(&format!("missing-{n}"))).collect();
        sent.send((panicked, errno("missing-0"))).unwrap();
    });
    let answered = answered.recv_timeout(std::time::Duration::from_secs(30));
    fail::remove("fuse::lookup");
    let (panicked, after) = answered.expect("the mount stopped answering");
    looker.join().unwrap();
    assert_eq!(panicked, vec![Some(libc::EIO); 9]);
    assert_eq!(after, Some(libc::ENOENT), "a lookup after the panics");
    mount.unmount(true).unwrap();
    block_on(mount.join().unwrap().check()).unwrap();
}

/// Board 8539: a tenant whose cells' requests wait, as on a bucket that never answers, holds at
/// most all the mount's workers but two: another tenant's cell's lookup, and a listing of the
/// mount's root, are answered meanwhile. Until then the waiting requests of one cell took every
/// worker, and the mount answered nothing, of any tenant, until they ended.
#[test]
fn one_tenant_waiting_holds_no_other_tenants_requests() {
    use std::sync::{Arc, Condvar, Mutex};
    /// Held requests' count, and whether they are let go.
    type Held = Arc<(Mutex<(usize, bool)>, Condvar)>;
    /// Lets the held requests go, and holds no more, however the test ends: a held request no
    /// one let go would hold the mount's unmount for good.
    struct LetGo(Held);
    impl Drop for LetGo {
        fn drop(&mut self) {
            let (state, changed) = &*self.0;
            state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).1 = true;
            changed.notify_all();
            fail::remove("fuse::tenant::slow");
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let m = dir.path().join(".gvfs");
    std::fs::create_dir(&m).unwrap();
    let mount = MountOptions::new().mount_cells(&m).unwrap();
    for (name, cell) in [("slow/a", "a"), ("fast/b", "b")] {
        let fs = format(&dir.path().join(cell));
        block_on(mount.server().attach(name, std::future::ready(Ok(fs)))).unwrap();
    }
    // Looked up once, so that the lookups below revalidate names the kernel has, which go to the
    // mount at once, where a name's first lookups wait in the kernel on one.
    std::fs::metadata(m.join("slow/a")).unwrap();
    // Each request of tenant `slow` held, counted, until let go: its lookups of its directory's
    // name among them, which read its cells' roots (hotbed review 57).
    let held: Held = Arc::new((Mutex::new((0usize, false)), Condvar::new()));
    let holding = held.clone();
    fail::cfg_callback("fuse::tenant::slow", move || {
        let (state, changed) = &*holding;
        let mut state = state.lock().unwrap();
        state.0 += 1;
        changed.notify_all();
        while !state.1 {
            state = changed.wait(state).unwrap();
        }
    })
    .unwrap();
    let let_go = LetGo(held.clone());
    let waiting: Vec<_> = (0..16)
        .map(|n| {
            let missing = m.join("slow/a").join(format!("missing-{n}"));
            std::thread::spawn(move || std::fs::metadata(missing).unwrap_err().raw_os_error())
        })
        .collect();
    {
        let (state, changed) = &*held;
        let state = state.lock().unwrap();
        let timeout = std::time::Duration::from_secs(30);
        let (state, _) = changed.wait_timeout_while(state, timeout, |state| state.0 < 6).unwrap();
        assert_eq!(state.0, 6, "slow's requests held");
    }

    let (sent, answered) = std::sync::mpsc::channel();
    let listing = m.clone();
    std::thread::spawn(move || {
        let fast = std::fs::metadata(listing.join("fast/b/missing")).unwrap_err().raw_os_error();
        sent.send((fast, names(&listing))).unwrap();
    });
    let answered = answered.recv_timeout(std::time::Duration::from_secs(1));
    let held_then = held.0.lock().unwrap().0;
    drop(let_go);
    let still: Vec<_> = waiting.into_iter().map(|thread| thread.join().unwrap()).collect();
    assert_eq!(answered, Ok((Some(libc::ENOENT), vec!["fast".to_owned(), "slow".to_owned()])));
    assert_eq!(held_then, 6, "the most of slow's requests in service");
    assert_eq!(still, vec![Some(libc::ENOENT); 16]);
    mount.unmount(true).unwrap();
    mount.join().unwrap();
}

/// Board 8537: a daemon killed with its mount of cells leaves it in the kernel, its connection
/// aborted, answering `ENOTCONN`: the next daemon to mount there detaches it, a cladefs's, and
/// mounts anew. Until then mount(2) failed with `ENOTCONN`, and no daemon started until someone
/// ran `fusermount3 -uz`.
#[test]
fn a_daemon_mounts_over_the_mount_of_one_killed() {
    let dir = tempfile::tempdir().unwrap();
    let bucket = dir.path().join("bucket");
    let first = dir.path().join("first");
    std::fs::create_dir(&first).unwrap();
    let (mut killed, _, m) = cladefsd_mounting(&first, &bucket, 1);
    killed.0.kill().unwrap();
    killed.0.wait().unwrap();
    let dead = std::fs::metadata(&m).unwrap_err();
    assert_eq!(dead.raw_os_error(), Some(libc::ENOTCONN), "{dead}");

    let second = dir.path().join("second");
    std::fs::create_dir(&second).unwrap();
    let socket = second.join("control");
    let mut command = cladefsd(&second.join("vm.sock"), &bucket, 2);
    command
        .env("CLADEFSD_GRPC", &socket)
        .env("CLADEFSD_MOUNT", &m)
        .env("CLADEFSD_MOUNT_STORES", second.join("cells"));
    let child = serving(command, &socket);
    assert_eq!(names(&m), Vec::<String>::new());
    cladefsctl_mount(socket.to_str().unwrap(), &["create", "1", &CELL_SIZE.to_string()]);
    assert_eq!(names(&m), ["0000000000001"]);
    terminate(child, &m);
}
