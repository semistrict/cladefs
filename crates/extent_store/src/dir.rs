// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The file system a store lives in: a directory of the host's ([`HostDir`]), or of memory
//! ([`MemDir`](crate::MemDir)), which can simulate crashes.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A file of a store.
pub trait File: Send + Sync + 'static {
    /// Reads `buf.len()` bytes at `offset`. Bytes past the file's end read as zeros.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;

    /// cladefs: reads as [`File::read_at`] does, but, on Linux, through the host's page cache, which
    /// `read_at` bypasses. The store reads its raw files so: the open segment's, those of segments
    /// being framed or filled, and open extents' own. It wrote them lately, through the cache,
    /// which holds their pages still, dirty or not yet let go; a read bypassing the cache writes
    /// back the dirty ones it reads first, waiting for the host's disk. Under pgbench's load, a
    /// commit copying the extents written in pieces waited about 4 ms a MiB for it, up to 400 ms,
    /// holding every write back; and a closed segment's framing, reading its 20-35 MiB, took a
    /// median 100-190 ms to read them, 6-19 ms through the cache (TASK-91.17, TASK-79.93). On
    /// macOS this reads around the cache as `read_at` does: a file's two descriptors there share
    /// `F_NOCACHE`.
    fn read_cached(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.read_at(buf, offset)
    }

    /// Writes `buf` at `offset`.
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;

    /// cladefs: has the host write the `len` bytes at `offset` back to its disk, written through its
    /// page cache, and waits for them; durable only once synced. A writer of a large file writes
    /// it back so, a piece at a time, as it writes, that the sync after has little left: a sync of
    /// a whole framed segment's data at once filled the disk's queue, which the syncs a daemon's
    /// turn makes its guests' commits durable with waited behind, up to a second under pgbench
    /// (GCE r728, TASK-165.9). A host that writes back as it pleases does nothing.
    fn write_back(&self, _offset: u64, _len: u64) -> io::Result<()> {
        Ok(())
    }

    /// cladefs: has the host begin writing the `len` bytes at `offset` back to its disk, written
    /// through its page cache, waiting for none of it; durable only once synced. A file written
    /// as a guest writes, as the open segment's raw file is, is written back so behind its writes,
    /// that the disk writes as they come rather than all at once at the sync after (TASK-221). A
    /// host that writes back as it pleases does nothing.
    fn write_behind(&self, _offset: u64, _len: u64) -> io::Result<()> {
        Ok(())
    }

    /// The file's size.
    fn size(&self) -> io::Result<u64>;

    /// Makes what was written to the file durable.
    fn sync(&self) -> io::Result<()>;

    /// cladefs: makes what was written to the file durable, as [`File::sync`] does, but leaves its
    /// pages in the host's page cache, if they are there: for a file written and synced again and
    /// again, as the open segment's raw file is, whose pages go with it once it is removed
    /// (TASK-92.23).
    fn sync_cached(&self) -> io::Result<()>;
}

/// The directory a store lives in, with what the store needs of its file system. Paths are
/// relative to it: a file's name, or a subdirectory's and a file's, joined by `/`. `""` is the
/// directory itself.
pub trait Dir: Send + Sync + 'static {
    type File: File;

    /// Creates the file at `path`, which must not exist, for reading and writing. `data` says
    /// whether it holds extents, whose reads and writes are block-aligned, and which the host may
    /// keep out of its page cache.
    fn create(&self, path: &str, data: bool) -> io::Result<Self::File>;

    /// Opens the file at `path` for reading and writing; `data` as for `create`.
    fn open(&self, path: &str, data: bool) -> io::Result<Self::File>;

    /// Removes the file at `path`.
    fn remove(&self, path: &str) -> io::Result<()>;

    /// cladefs: renames the file at `from` to `to`, which is not there, at once: in one directory, a
    /// crash leaves it at one or the other, as `sync_dir` makes durable (TASK-301); from one
    /// directory to another, its entry in each is durable once that directory is synced, so that a
    /// crash may leave it at both, or at neither (TASK-310). By default, unsupported.
    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let _ = (from, to);
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }

    /// cladefs: makes `to`, which is not there, another name of the file at `from`, which keeps its
    /// own (a hard link): `to`'s entry durable once its directory is synced. What a segment framed
    /// in place is put among framed ones by, its raw file then its framed file, the raw name going
    /// once its commits are durable (TASK-310). By default, unsupported: segments are framed in
    /// copies then.
    fn link(&self, from: &str, to: &str) -> io::Result<()> {
        let _ = (from, to);
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }

    /// Creates the directory at `path`.
    fn create_dir(&self, path: &str) -> io::Result<()>;

    /// The names of the files in the directory at `path`.
    fn list(&self, path: &str) -> io::Result<Vec<String>>;

    /// Makes the directory's entries durable: the files created and removed in it.
    fn sync_dir(&self, path: &str) -> io::Result<()>;

    /// Makes everything synced durable, past any cache the host's storage keeps.
    fn barrier(&self) -> io::Result<()>;

    /// Takes the directory for this process, against other processes' takings of it: what a
    /// process holds while it has branches of the store open, or opens or deletes them, so that
    /// two processes never change one store at once. It waits a while for another process to let
    /// it go, as one whose last branch is closing does. A directory no other process sees takes
    /// nothing.
    fn take(&self) -> io::Result<()> {
        Ok(())
    }

    /// Lets the directory go, once taken ([`Dir::take`]).
    fn let_go(&self) {}

    /// cladefs: claims the directory's store for one store open in this process: what refuses a
    /// second opening of it, or its deletion while open, at once, rather than wait for itself
    /// (TASK-165.7). A directory no other opening sees claims nothing.
    fn claim(&self) -> io::Result<()> {
        Ok(())
    }

    /// cladefs: lets the claim go, once claimed ([`Dir::claim`]).
    fn unclaim(&self) {}

    /// cladefs: the path on the host's file system of the file at `path`, if it is on it: what a
    /// store's file leaving it for the host's cache of objects is renamed from (TASK-165.8).
    fn host_path(&self, _path: &str) -> Option<PathBuf> {
        None
    }

    /// cladefs: whether the store may do I/O in the directory on threads of its own, alongside its
    /// callers': framing a closed segment off the commit that closed it (TASK-41), and syncing a
    /// flush's files at once (TASK-92.42). Without, its callers' threads do all of it, one
    /// operation at a time, in an order their calls alone decide: as a directory needs that fails
    /// from a number of operations on, as a crash would, at the same point each time it is run
    /// ([`MemDir::serial`](crate::MemDir::serial), TASK-79.79).
    fn concurrent(&self) -> bool {
        true
    }
}

/// The whole of `file`, read as it is: a store's file that holds no extents' data, whose reads
/// need not be aligned.
pub(crate) fn read_file<F: File>(file: &F) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; file.size()? as usize];
    file.read_at(&mut bytes, 0)?;
    Ok(bytes)
}

/// Whether `err` says a file is not there.
pub(crate) fn not_found(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::NotFound
}

/// How long [`Dir::take`] waits for another process to let a directory of the host's go.
const TAKE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// A directory of the host's.
///
/// cladefs: every file of it bypasses the host's page cache (`O_DIRECT` on Linux, `F_NOCACHE` on
/// macOS), or is not opened: a file system that can't fails its openings (TASK-433.6). Its reads
/// and writes go by the blocks around them, from memory aligned to them
/// (`extent_bucket::direct`), so that no byte of a store's is held in
/// the host's page cache, a memory no budget of the process's counts (TASK-433). Extents' files
/// go through the process's page cache instead ([`crate::pages`]): a write lands there and
/// returns, the disk taking it later, coalesced, as the host's page cache had it, a `WRITE`
/// waiting for no disk, and what was written lately read back from memory, as framings and
/// commits copying extents read it (TASK-91.17, TASK-221, TASK-401); and every opening of a file,
/// by whatever name, shares its pages. A file removed drops its pages, its dirty ones unwritten.
/// Until TASK-433, they were written through the host's page cache, read around it, and synced
/// keeping the open segment's pages there (TASK-92.23). Syncing a file is `fdatasync` on Linux,
/// and a directory's entries `fsync`, each of which flushes the drive's cache too. cladefs: on macOS
/// each is an `F_BARRIERFSYNC`, which leaves the drive's cache to `barrier`, an `F_FULLFSYNC`, but
/// orders what it synced before what is written after (TASK-92.48). They were std's `sync_data`
/// and `sync_all`, each an `F_FULLFSYNC` there, which flushed the drive's cache at every sync. Run
/// interleaved, twenty runs of each, by their medians and quartiles, store-bench's `burst`, whose
/// commit seals 640 extents in files of their own, flushed in 620 ms (572-664 ms), and 175 ms since
/// (168-209 ms), the host's load average 4.7-6.4; `durable`'s commits of one in 9.8 ms (7.7-14.9
/// ms), and 4.1 ms since (3.6-5.2 ms), the load 6.7-18: measured with `AT_ONCE`'s at those loads.
/// The trade-off: a sync on macOS is durable as it returns against the store's process or the
/// host's kernel crashing, as on Linux, but against the drive losing power only once a barrier
/// follows; and each point at which the store says that what it wrote is durable ends in one: a
/// flush, a closing between commits, a branch's record, an index's first checkpoint. A plain
/// `fsync` would leave the order to the drive as well, so that a power loss could keep a commit's
/// record, synced after its data, without the data. macOS is a development host: stores run on
/// Linux.
pub type HostDir = crate::CachedDir<DiskDir>;

/// cladefs: a file of a [`HostDir`]: one of its extents' files, through the process's page cache,
/// or any other, read and written as it is ([`DiskFile`]).
pub type HostFile = crate::CachedFile<DiskFile>;

impl HostDir {
    /// The directory at `path`, which exists, its extents' files through the process's page cache
    /// ([`crate::pages::shared`]). Its files are opened bypassing the host's page cache, or not
    /// at all: a file system that refuses to fails their openings (`InvalidInput`), never falling
    /// back to the host's cache, a second memory (TASK-433, TASK-433.6).
    pub fn new(path: &Path) -> io::Result<Self> {
        Ok(Self::over(DiskDir::new(path)?, crate::pages::shared().clone()))
    }

    /// The directory's path.
    pub fn path(&self) -> &Path {
        &self.under().path
    }
}

/// cladefs: a directory of the host's, its files bypassing the host's page cache: what a
/// [`HostDir`] holds its extents' files in a page cache of the process's over.
pub struct DiskDir {
    path: PathBuf,
    /// The directory, open, for `barrier`, and for the `flock` taking it, which outlives this
    /// while other takings of it in the process do ([`Dir::take`]).
    handle: Arc<std::fs::File>,
}

impl DiskDir {
    /// The directory at `path`, which exists.
    pub fn new(path: &Path) -> io::Result<Self> {
        let handle = Arc::new(std::fs::File::open(path)?);
        Ok(Self { path: path.to_owned(), handle })
    }

    /// `file`, opened at `path`, bypassing the host's page cache, or refused, naming it, if its
    /// file system refuses that (`InvalidInput`, [`extent_bucket::direct::bypass`]).
    fn file(&self, path: &str, file: std::fs::File) -> io::Result<DiskFile> {
        extent_bucket::direct::bypass(&file, &self.path.join(path))?;
        Ok(DiskFile { file, tail: Mutex::default() })
    }
}

impl Dir for DiskDir {
    type File = DiskFile;

    fn create(&self, path: &str, _data: bool) -> io::Result<DiskFile> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(self.path.join(path))?;
        self.file(path, file)
    }

    fn open(&self, path: &str, _data: bool) -> io::Result<DiskFile> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(self.path.join(path))?;
        self.file(path, file)
    }

    fn remove(&self, path: &str) -> io::Result<()> {
        std::fs::remove_file(self.path.join(path))
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        std::fs::rename(self.path.join(from), self.path.join(to))
    }

    fn link(&self, from: &str, to: &str) -> io::Result<()> {
        std::fs::hard_link(self.path.join(from), self.path.join(to))
    }

    fn create_dir(&self, path: &str) -> io::Result<()> {
        std::fs::create_dir(self.path.join(path))
    }

    fn list(&self, path: &str) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(self.path.join(path))? {
            let name = entry?.file_name();
            names.push(name.into_string().map_err(|name| {
                io::Error::new(io::ErrorKind::InvalidData, format!("{name:?} isn't UTF-8"))
            })?);
        }
        Ok(names)
    }

    fn host_path(&self, path: &str) -> Option<PathBuf> {
        Some(self.path.join(path))
    }

    fn sync_dir(&self, path: &str) -> io::Result<()> {
        let dir = std::fs::File::open(self.path.join(path))?;
        #[cfg(target_os = "macos")]
        {
            sync_with(&dir, &[libc::F_BARRIERFSYNC, libc::F_FULLFSYNC])
        }
        #[cfg(not(target_os = "macos"))]
        {
            dir.sync_all()
        }
    }

    /// Takes the directory with an exclusive `flock` of it, which a descriptor of the directory's
    /// of the process's holds until the process lets it go, or exits. cladefs: the process's
    /// takings of it share that, counted, each `HostDir` of it taking it once: what is taken in
    /// this process another opening of it here shares, as a store's uploader stopping, which
    /// holds it ([`Store::hold_dir`](crate::Store::hold_dir)), and its store's next opening do;
    /// which store is open here, its claim says ([`Dir::claim`], TASK-165.7).
    fn take(&self) -> io::Result<()> {
        let id = self.id()?;
        let mut taken = TAKEN.lock().unwrap();
        if let Some((count, _)) = taken.get_mut(&id) {
            *count += 1;
            return Ok(());
        }
        // Its descriptor, which holds the `flock` while any taking in the process does.
        take_from_others(&self.handle)?;
        taken.insert(id, (1, self.handle.clone()));
        Ok(())
    }

    fn let_go(&self) {
        let id = match self.id() {
            Ok(id) => id,
            Err(err) => {
                return log::warn!("extent store: letting {} go: {err}", self.path.display());
            }
        };
        let mut taken = TAKEN.lock().unwrap();
        let Some((count, _)) = taken.get_mut(&id) else { return };
        *count -= 1;
        if *count > 0 {
            return;
        }
        let (_, lock) = taken.remove(&id).unwrap();
        // SAFETY: Plain system call on the directory's descriptor.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) } != 0 {
            let err = io::Error::last_os_error();
            log::warn!("extent store: unlocking {}: {err}", self.path.display());
        }
    }

    fn claim(&self) -> io::Result<()> {
        if !CLAIMED.lock().unwrap().insert(self.id()?) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "this process has the store open",
            ));
        }
        Ok(())
    }

    fn unclaim(&self) {
        match self.id() {
            Ok(id) => drop(CLAIMED.lock().unwrap().remove(&id)),
            Err(err) => log::warn!("extent store: unclaiming {}: {err}", self.path.display()),
        }
    }

    fn barrier(&self) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            sync_with(&self.handle, &[libc::F_FULLFSYNC])
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = &self.handle;
            Ok(())
        }
    }
}

/// cladefs: the directories this process has taken ([`Dir::take`]), by their devices and inodes:
/// how many takings of each, and the descriptor holding the `flock`.
static TAKEN: Mutex<BTreeMap<DirId, (usize, Arc<std::fs::File>)>> = Mutex::new(BTreeMap::new());

/// cladefs: the directories whose stores this process has open ([`Dir::claim`]).
static CLAIMED: Mutex<BTreeSet<DirId>> = Mutex::new(BTreeSet::new());

/// A directory's device and inode, which name it however a path names it.
type DirId = (u64, u64);

impl DiskDir {
    /// The directory's device and inode.
    fn id(&self) -> io::Result<DirId> {
        use std::os::unix::fs::MetadataExt as _;
        let metadata = self.handle.metadata()?;
        Ok((metadata.dev(), metadata.ino()))
    }
}

/// Takes the directory through `lock`, a descriptor of it, against other processes' takings of it,
/// waiting a while for them to let it go ([`Dir::take`]).
fn take_from_others(lock: &std::fs::File) -> io::Result<()> {
    let deadline = std::time::Instant::now() + TAKE_WAIT;
    loop {
        // SAFETY: Plain system call on the directory's descriptor.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::WouldBlock {
            return Err(err);
        }
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "another process has the store's branches open",
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// cladefs: a file of the host's disk, bypassing its page cache ([`DiskDir`]): read and written by
/// the blocks around what is asked, from memory aligned to them, the block ending the last write
/// held, so that an append, a log's, reads nothing back (`extent_bucket::direct`, TASK-433).
pub struct DiskFile {
    file: std::fs::File,
    tail: Mutex<extent_bucket::direct::Tail>,
}

impl AsRawFd for DiskFile {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

impl File for DiskFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        extent_bucket::direct::read_at(&self.file, buf, offset)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        let mut tail = self.tail.lock().unwrap();
        extent_bucket::direct::write_at(&self.file, &mut tail, buf, offset)
    }

    /// Taken as no write is in flight: a write of part of a block writes it whole, the file cut
    /// back after, which a size taken between would see past the file's end.
    fn size(&self) -> io::Result<u64> {
        let _writes = self.tail.lock().unwrap();
        Ok(self.file.metadata()?.len())
    }

    fn sync(&self) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            sync_with(&self.file, &[libc::F_BARRIERFSYNC, libc::F_FULLFSYNC])
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.file.sync_data()
        }
    }

    fn sync_cached(&self) -> io::Result<()> {
        self.sync()
    }
}

/// cladefs: syncs `file` on macOS by the first of `syncs`, `fcntl`s of macOS's, that its file system
/// supports; or, if none, by an `fsync`, which is all there is then (TASK-92.48). Not std's
/// `sync_all`, which is an `F_FULLFSYNC` there.
#[cfg(target_os = "macos")]
fn sync_with(file: &std::fs::File, syncs: &[libc::c_int]) -> io::Result<()> {
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

/// cladefs: what `call`, a system call that returns -1 if it fails, gave, called again while a
/// signal interrupts it, as std's own calls are.
#[cfg(target_os = "macos")]
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
