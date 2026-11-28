// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the host's file system, as cladefsd serves from it (doc-24, TASK-685.4).
//!
//! What the daemon keeps on its host's disk, its cell directories, replica directories, part cache,
//! host state, it reads and writes through this crate rather than `std::fs`. Built normally, the
//! crate wraps `std::fs` in inline forwarders, which cost the daemon nothing in release builds
//! while asserting in debug and test builds that slow disk calls never run directly on an async
//! runtime thread outside [`may_block`] (board 10824), with the calls on a file's descriptor the
//! daemon makes beside it ([`punch_hole`], [`data_ranges`], [`Flock`]) and what `Path` asks the
//! file system itself ([`exists`], [`is_dir`]).
//!
//! Under `--cfg madsim`, each simulated host has a disk of its own in memory, of the same calls,
//! which keeps what a host's file system would have made durable: a file's contents as of its
//! last sync, a directory's entries as of its last sync, and directories once made ([`mem`], the
//! crash model `local_cell::MemDir` is of too). A host started again, killed before,
//! finds its disk as a crash left it, at its first call since: what was durable, and, as the
//! seed says, any of what happened since, in order, each block written since its file's last sync
//! there whole, not at all, garbage, or torn (FoundationDB's simulation's power losses do as much).
//! A host's locks of files ([`Flock`]) are its disk's, and go with its crash, as a process's go
//! with its death.

mod blocking;
pub mod crash;
pub mod mem;

use blocking::assert_may_block;
pub use blocking::may_block;

#[cfg(not(madsim))]
mod real;
#[cfg(not(madsim))]
pub use real::{
    DirEntry, File, FileExt, FileType, Metadata, MetadataExt, OpenOptions, OpenOptionsExt, ReadDir,
    canonicalize, copy, create_dir, create_dir_all, hard_link, metadata, read, read_dir,
    read_to_string, remove_dir, remove_dir_all, remove_file, rename, symlink_metadata, write,
};

#[cfg(madsim)]
mod sim;
#[cfg(madsim)]
pub use sim::{
    DirEntry, File, FileType, Metadata, OpenOptions, ReadDir, canonicalize, copy, create_dir,
    create_dir_all, hard_link, metadata, read, read_dir, read_to_string, remove_dir,
    remove_dir_all, remove_file, rename, symlink_metadata, write,
};
#[cfg(madsim)]
pub use sim::{FileExt, MetadataExt, OpenOptionsExt, faults, lose};

mod flock;
pub use flock::Flock;

use std::io;
use std::path::Path;

/// Whether something is at `path`, as `Path::exists` says of the host's file system.
pub fn exists(path: impl AsRef<Path>) -> bool {
    metadata(path).is_ok()
}

/// Whether a directory is at `path`, as `Path::is_dir` says.
pub fn is_dir(path: impl AsRef<Path>) -> bool {
    metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// Whether a file is at `path`, as `Path::is_file` says.
pub fn is_file(path: impl AsRef<Path>) -> bool {
    metadata(path).is_ok_and(|metadata| metadata.is_file())
}

/// Makes the directory at `path` and those above it, as `create_dir_all` does, each made durable
/// in its parent: its parent synced once it is made, so that a crash keeps what a daemon keeps in
/// it, as it does what it promised there (TASK-708).
pub fn create_dir_all_durably(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    if is_dir(path) {
        return Ok(());
    }
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        create_dir_all_durably(parent)?;
    }
    match create_dir(path) {
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists && is_dir(path) => return Ok(()),
        made => made?,
    }
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty());
    sync_dir(parent.unwrap_or(Path::new(".")))
}

/// Makes the entries of the directory at `path` durable, as an `fsync` of it does.
pub fn sync_dir(path: impl AsRef<Path>) -> io::Result<()> {
    File::open(path)?.sync_all()
}

/// Gives the `len` bytes at `offset` of `file` back to its disk, keeping its size: `fallocate`'s
/// `FALLOC_FL_PUNCH_HOLE` on Linux, `F_PUNCHHOLE` on macOS. The bytes read as zeros after.
#[track_caller]
pub fn punch_hole(file: &File, offset: u64, len: u64) -> io::Result<()> {
    assert_may_block("punch_hole");
    if len == 0 {
        return Ok(());
    }
    #[cfg(madsim)]
    return file.punch(offset, len);
    #[cfg(not(madsim))]
    return descriptor::punch_hole(file, offset, len);
}

/// The ranges of `file` below `size` that hold data, holes between them, in order: as `lseek`'s
/// `SEEK_DATA` and `SEEK_HOLE` find them; the whole of it on a file system that finds none
/// (`EINVAL`).
#[track_caller]
pub fn data_ranges(file: &File, size: u64) -> io::Result<Vec<std::ops::Range<u64>>> {
    assert_may_block("data_ranges");
    #[cfg(madsim)]
    return file.data_ranges(size);
    #[cfg(not(madsim))]
    return descriptor::data_ranges(file, size);
}

/// The bytes of `file` its disk holds, holes not counted: its blocks', as `stat` counts them.
#[track_caller]
pub fn allocated(file: &File) -> io::Result<u64> {
    Ok(file.metadata()?.blocks() * 512)
}

/// Syncs `file` on macOS by the first of `syncs`, `fcntl`s of macOS's (`F_BARRIERFSYNC`,
/// `F_FULLFSYNC`), that its file system supports; or, if none, by an `fsync`, which is all there is
/// then (TASK-92.48). Not std's `sync_all`, which is an `F_FULLFSYNC` there. Under madsim, a sync.
#[cfg(target_os = "macos")]
#[track_caller]
pub fn sync_with(file: &File, syncs: &[libc::c_int]) -> io::Result<()> {
    assert_may_block("sync_with");
    #[cfg(madsim)]
    {
        let _ = syncs;
        file.sync_all()
    }
    #[cfg(not(madsim))]
    descriptor::sync_with(file, syncs)
}

/// The calls on a file's descriptor, built normally.
#[cfg(not(madsim))]
mod descriptor {
    use crate::File;
    use std::io;
    use std::os::fd::AsRawFd as _;

    pub(super) fn punch_hole(file: &File, offset: u64, len: u64) -> io::Result<()> {
        let fd = file.as_raw_fd();
        let (offset, len) = (
            libc::off_t::try_from(offset).map_err(|_| io::ErrorKind::InvalidInput)?,
            libc::off_t::try_from(len).map_err(|_| io::ErrorKind::InvalidInput)?,
        );
        #[cfg(target_os = "linux")]
        {
            let mode = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
            // SAFETY: Plain system call on an open descriptor.
            again_if_interrupted(|| unsafe { libc::fallocate(fd, mode, offset, len) })
        }
        #[cfg(target_os = "macos")]
        {
            let hole =
                libc::fpunchhole_t { fp_flags: 0, reserved: 0, fp_offset: offset, fp_length: len };
            // SAFETY: Plain system call on an open descriptor, with a hole it reads.
            again_if_interrupted(|| unsafe { libc::fcntl(fd, libc::F_PUNCHHOLE, &hole) })
        }
    }

    pub(super) fn data_ranges(file: &File, size: u64) -> io::Result<Vec<std::ops::Range<u64>>> {
        let seek = |at: u64, whence| {
            // SAFETY: Plain system call on an open descriptor.
            match unsafe { libc::lseek(file.as_raw_fd(), at as libc::off_t, whence) } {
                -1 => Err(io::Error::last_os_error()),
                found => Ok(found as u64),
            }
        };
        let mut ranges = Vec::new();
        let mut at = 0;
        while at < size {
            let start = match seek(at, libc::SEEK_DATA) {
                Ok(start) => start,
                // No data past `at`.
                Err(err) if err.raw_os_error() == Some(libc::ENXIO) => break,
                // A file system that finds no holes: all of it data.
                Err(err) if err.raw_os_error() == Some(libc::EINVAL) => {
                    ranges.clear();
                    ranges.push(0..size);
                    return Ok(ranges);
                }
                Err(err) => return Err(err),
            };
            let end = seek(start, libc::SEEK_HOLE)?.min(size);
            if end <= start {
                break;
            }
            ranges.push(start..end);
            at = end;
        }
        Ok(ranges)
    }

    #[cfg(target_os = "macos")]
    pub(super) fn sync_with(file: &File, syncs: &[libc::c_int]) -> io::Result<()> {
        let fd = file.as_raw_fd();
        for &sync in syncs {
            // SAFETY: Flushes an open file descriptor's file, as `sync` says.
            let synced = again_if_interrupted(|| unsafe { libc::fcntl(fd, sync) });
            // A file system without it says so.
            let unsupported = matches!(
                synced.as_ref().map_err(io::Error::raw_os_error),
                Err(Some(libc::ENOTSUP | libc::ENOTTY | libc::EINVAL))
            );
            if !unsupported {
                return synced;
            }
        }
        // SAFETY: Syncs an open file descriptor's file.
        again_if_interrupted(|| unsafe { libc::fsync(fd) })
    }

    /// Calls `call`, a system call that returns -1 as it fails, again as long as a signal
    /// interrupts it.
    fn again_if_interrupted(call: impl Fn() -> libc::c_int) -> io::Result<()> {
        loop {
            if call() != -1 {
                return Ok(());
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }
}

#[cfg(all(test, not(madsim), debug_assertions))]
mod blocking_tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    /// Board 10824: calling a slow `host_fs` operation directly on a tokio worker panics in
    /// debug/test builds, while calling it on a plain thread or inside `may_block` succeeds,
    /// including nested scopes and scopes unwound by a panic.
    #[test]
    fn slow_calls_panic_on_async_workers_unless_inside_may_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");

        // Plain OS thread: allowed without `may_block`.
        write(&path, b"hello").unwrap();
        assert_eq!(read(&path).unwrap(), b"hello");

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        // Direct call on a tokio worker panics (free function).
        let p = path.clone();
        let err = runtime.block_on(runtime.spawn(async move {
            let _ = metadata(&p);
        }));
        assert!(err.unwrap_err().is_panic(), "host_fs::metadata on a tokio worker must panic");

        // Direct call on an already-open `File` on a tokio worker panics too.
        let file = File::options().read(true).write(true).open(&path).unwrap();
        let err = runtime.block_on(runtime.spawn(async move {
            let _ = file.sync_all();
        }));
        assert!(
            err.unwrap_err().is_panic(),
            "host_fs::File::sync_all on a tokio worker must panic"
        );

        // Inside `may_block` (including nested scopes) on a tokio worker: allowed.
        let p = path.clone();
        runtime
            .block_on(runtime.spawn(async move {
                may_block(|| {
                    assert!(exists(&p));
                    let mut file =
                        may_block(|| File::options().read(true).write(true).open(&p).unwrap());
                    let mut buf = [0u8; 5];
                    file.read_exact_at(&mut buf, 0).unwrap();
                    assert_eq!(&buf, b"hello");
                    file.write_all(b"world").unwrap();
                    file.sync_data().unwrap();
                    let mut back = Vec::new();
                    let mut reader = File::open(&p).unwrap();
                    reader.read_to_end(&mut back).unwrap();
                    assert_eq!(back, b"world");
                });
            }))
            .unwrap();

        // A panic inside `may_block` restores the depth so a later unguarded call still panics.
        let p = path.clone();
        let err = runtime.block_on(runtime.spawn(async move {
            let caught = std::panic::catch_unwind(|| {
                may_block(|| panic!("boom"));
            });
            assert!(caught.is_err());
            let _ = exists(&p);
        }));
        assert!(
            err.unwrap_err().is_panic(),
            "host_fs::exists after an unwound may_block scope must still panic"
        );
    }
}
