// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The file system a cell lives in: a directory of the host's ([`HostDir`]), or of memory
//! ([`MemDir`](crate::MemDir)), which can simulate crashes.

use host_fs::OpenOptions;
use host_fs::OpenOptionsExt as _;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A file of a cell.
pub trait File: Send + Sync + 'static {
    /// Reads `buf.len()` bytes at `offset`. Bytes past the file's end read as zeros.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;

    /// Writes `buf` at `offset`.
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;

    /// cladefs: has the host write the `len` bytes at `offset` back to its disk, written through
    /// its page cache, and waits for them; durable only once synced. A writer of a large file
    /// writes it back so, a piece at a time, as it writes, that the sync after has little left: a
    /// sync of a whole framed segment's data at once filled the disk's queue, which the syncs a
    /// daemon's turn makes its guests' commits durable with waited behind, up to a second under
    /// pgbench (GCE r728, TASK-165.9). A host that writes back as it pleases does nothing.
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

    /// cladefs: gives the `len` bytes at `offset`, whole blocks, back to the host's disk, keeping
    /// the file's size: they read as zeros from now on (doc-25, TASK-698.2). An ephemeral cell's
    /// kept segments give back so the runs of the extents freed in them, which nothing reads again.
    /// A file that can't refuses (`Unsupported`), giving nothing back.
    fn punch(&self, _offset: u64, _len: u64) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }

    /// cladefs: the bytes of the host's disk the file takes: its size, but for what [`File::punch`]
    /// gave back (doc-25, TASK-698.2), as its footprint counts it.
    fn allocated(&self) -> io::Result<u64> {
        self.size()
    }
}

/// cladefs: a file or directory a directory holds, as [`Dir::entries`] lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirFile {
    /// Its path in the directory.
    pub path: String,
    /// Its length, in bytes: nothing, for a directory.
    pub len: u64,
    /// cladefs: the bytes of the host's disk it takes, but for what was given back
    /// ([`File::punch`]): nothing, for a directory (doc-25, TASK-698.2).
    pub allocated: u64,
    /// Its inode, which its every name shares: nothing, for a directory.
    pub inode: u64,
    /// Whether it is a directory.
    pub dir: bool,
}

/// The directory a cell lives in, with what the cell needs of its file system. Paths are
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

    /// cladefs: renames the file at `from` to `to`, which is not there, at once: in one directory,
    /// a crash leaves it at one or the other, as `sync_dir` makes durable (TASK-301); from one
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

    /// cladefs: writes `bytes` as the file at `path`, whole or not at all: a file written and
    /// synced beside it, renamed over any there, at once, as [`Dir::rename`] renames, so that a
    /// crash leaves the one there before or this, never neither, and this once its directory is
    /// synced (TASK-539.3). Failing, it changed nothing at `path`. What the records beside a cell's
    /// files are written by: its description, and cladefs's own, each synced so.
    fn replace(&self, path: &str, bytes: &[u8]) -> io::Result<()>;

    /// Creates the directory at `path`.
    fn create_dir(&self, path: &str) -> io::Result<()>;

    /// The names of the files in the directory at `path`.
    fn list(&self, path: &str) -> io::Result<Vec<String>>;

    /// cladefs: every file and directory it holds, at any depth, but itself: what a cell's
    /// footprint counts as its first opening begins ([`crate::footprint`], TASK-539.4). By
    /// default, unsupported.
    fn entries(&self) -> io::Result<Vec<DirFile>> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }

    /// Makes the directory's entries durable: the files created and removed in it.
    fn sync_dir(&self, path: &str) -> io::Result<()>;

    /// Makes everything synced durable, past any cache the host's storage keeps.
    fn barrier(&self) -> io::Result<()>;

    /// cladefs: whether its syncs are skipped now, its durability deferred by its maker
    /// ([`crate::defer_durability`], board 7420).
    fn defers(&self) -> bool {
        false
    }

    /// cladefs: whether it is whole in this process, unmade on the disk as its durability is
    /// deferred ([`crate::defer_durability`]).
    fn made_deferred(&self) -> bool {
        false
    }

    /// cladefs: makes it durable, its durability deferred ([`crate::defer_durability`]): what
    /// a cell's first flush and its close do first.
    fn settle(&self) -> io::Result<()> {
        Ok(())
    }

    /// Takes the directory, held while its cell is open, or opened or deleted, counted in the
    /// process. cladefs: no other process takes a cell's directory: one extent daemon owns a
    /// host's disks, locking each disk's root as it boots (cladefsd's `disks`), and what else
    /// works on a cell does so through it (the user, 2026-11-26, board 14305); each cell's flock
    /// went, and the descriptor its lock held. A directory no other process sees takes nothing.
    fn take(&self) -> io::Result<()> {
        Ok(())
    }

    /// Lets the directory go, once taken ([`Dir::take`]).
    fn let_go(&self) {}

    /// cladefs: claims the directory's cell for one cell open in this process: what refuses a
    /// second opening of it, or its deletion while open, at once, rather than wait for itself
    /// (TASK-165.7). One claimed by a cell closing it waits for, a while: a cell let go of in
    /// this process closes on whatever thread drops it last, its last commits made durable, which
    /// takes seconds on a busy disk, and an opening of it then is no second opening (TASK-648). A
    /// directory no other opening sees claims nothing.
    fn claim(&self) -> io::Result<()> {
        Ok(())
    }

    /// cladefs: says the cell claimed is closing ([`Dir::claim`]): an opening of it waits for its
    /// claim to go, rather than fail.
    fn closing(&self) {}

    /// cladefs: lets the claim go, once claimed ([`Dir::claim`]).
    fn unclaim(&self) {}

    /// cladefs: gives the file at `path` to `to`, by its path on the host's filesystem, which `to`
    /// takes it from, by a rename, or not: as a cell's file leaving it goes into the host's cache
    /// of objects (TASK-165.8). Returns whether `to` took it, which goes from the directory then,
    /// as a removal would take it; a directory not on the host's filesystem gives nothing.
    fn give(&self, path: &str, to: &mut dyn FnMut(&Path) -> io::Result<bool>) -> io::Result<bool> {
        let _ = (path, to);
        Ok(false)
    }

    /// cladefs: whether the cell may do I/O in the directory on threads of its own, alongside its
    /// callers': framing a closed segment off the commit that closed it (TASK-41), and syncing a
    /// flush's files at once (TASK-92.42). Without, its callers' threads do all of it, one
    /// operation at a time, in an order their calls alone decide: as a directory needs that fails
    /// from a number of operations on, as a crash would, at the same version each time it is run
    /// ([`MemDir::serial`](crate::MemDir::serial), TASK-79.79).
    fn concurrent(&self) -> bool {
        true
    }

    /// cladefs: the disk the directory is on, whose counters its cell's files charge and whose
    /// governor admits its writes against (doc-22): none for a directory no disk counts.
    fn disk(&self) -> Option<&Arc<bucket_cell::host_disk::HostDisk>> {
        None
    }

    /// cladefs: the footprint its cell's files are counted in, if they are ([`crate::Metered`]).
    fn footprint(&self) -> Option<&Arc<crate::footprint::Footprint>> {
        None
    }

    /// cladefs: the host context owning this directory's locks, page cache, footprints, and
    /// uploaders, if it is a host directory ([`HostDir`]).
    fn host(&self) -> Option<&Arc<crate::Host>> {
        None
    }
}

/// Maximum size of a cell's description (`store`) or small metadata file (board 9316).
pub const MAX_DESCRIPTION_BYTES: u64 = 64 << 10;

/// Maximum size of a WAL `start` (metadata + optional root) or replica `incarnation` file
/// (board 9316).
pub const MAX_START_BYTES: u64 = 1 << 20;

/// Maximum size of an ephemeral cell's `local-checkpoint` file (board 9316).
pub const MAX_LOCAL_CHECKPOINT_BYTES: u64 = 64 << 20;

/// Maximum size of a WAL or replica log file (board 9316).
pub const MAX_LOG_BYTES: u64 = 128 << 20;

/// Maximum size of a raw open-segment file read whole during recovery (board 9316).
pub const MAX_RAW_SEGMENT_BYTES: u64 = 1 << 30;

/// The whole of `file`, read as it is, refusing any file longer than `max` before allocating
/// (board 9316): a cell's file that holds no extents' data, whose reads need not be aligned.
pub(crate) fn read_file<F: File>(file: &F, max: u64) -> io::Result<Vec<u8>> {
    let size = file.size()?;
    if size > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("file length {size} exceeds limit {max}"),
        ));
    }
    let len = usize::try_from(size).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, format!("file length {size} overflows usize"))
    })?;
    let mut bytes = vec![0; len];
    file.read_at(&mut bytes, 0)?;
    Ok(bytes)
}

/// Whether `err` says a file is not there.
pub(crate) fn not_found(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::NotFound
}

/// A directory of the host's.
///
/// cladefs: every file of it bypasses the host's page cache (`O_DIRECT` on Linux, `F_NOCACHE` on
/// macOS), or is not opened: a file system that can't fails its openings (TASK-433.6). Its reads
/// and writes go by the blocks around them, from memory aligned to them (`bucket_cell::direct`), so
/// that no byte of a cell's is held in the host's page cache, a memory no budget of the process's
/// counts (TASK-433). Extents' files go through the process's page cache instead
/// ([`crate::pages`]): a write lands there and returns, the disk taking it later, coalesced, as the
/// host's page cache had it, a `WRITE` waiting for no disk, and what was written lately read back
/// from memory, as framings and commits copying extents read it (TASK-91.17, TASK-221, TASK-401);
/// and every opening of a file, by whatever name, shares its pages. A file removed drops its pages,
/// its dirty ones unwritten. Until TASK-433, they were written through the host's page cache, read
/// around it, and synced keeping the open segment's pages there (TASK-92.23). Syncing a file is
/// `fdatasync` on Linux, and a directory's entries `fsync`, each of which flushes the drive's cache
/// too. cladefs: on macOS each is an `F_BARRIERFSYNC`, which leaves the drive's cache to `barrier`,
/// an `F_FULLFSYNC`, but orders what it synced before what is written after (TASK-92.48). They were
/// std's `sync_data` and `sync_all`, each an `F_FULLFSYNC` there, which flushed the drive's cache
/// at every sync. Run interleaved, twenty runs of each, by their medians and quartiles,
/// cell-bench's `burst`, whose commit seals 640 extents in files of their own, flushed in 620 ms
/// (572-664 ms), and 175 ms since (168-209 ms), the host's load average 4.7-6.4; `durable`'s
/// commits of one in 9.8 ms (7.7-14.9 ms), and 4.1 ms since (3.6-5.2 ms), the load 6.7-18: measured
/// with `AT_ONCE`'s at those loads. The trade-off: a sync on macOS is durable as it returns against
/// the cell's process or the host's kernel crashing, as on Linux, but against the drive losing
/// power only once a barrier follows; and each point at which the cell says that what it wrote is
/// durable ends in one: a flush, a closing between commits, a cell's record, an index's first
/// checkpoint. A plain `fsync` would leave the order to the drive as well, so that a power loss
/// could keep a commit's record, synced after its data, without the data. macOS is a development
/// host: extent stores run on Linux.
///
/// cladefs: its changes counted, as it makes them, in its cell's footprint ([`crate::Metered`],
/// TASK-539.4).
pub type HostDir = crate::Metered<crate::CachedDir<DiskDir>>;

/// cladefs: a file of a [`HostDir`]: one of its extents' files, through the process's page cache,
/// or any other, read and written as it is ([`DiskFile`]); its writes counted.
pub type HostFile = crate::MeteredFile<crate::CachedFile<DiskFile>>;

impl HostDir {
    /// The directory at `path` on `host`, which exists, its extents' files through `host`'s page
    /// cache ([`crate::Host::pages`]). Its files are opened bypassing the host's page cache, or
    /// not at all: a file system that refuses to fails their openings (`InvalidInput`), never
    /// falling back to the host's cache, a second memory (TASK-433, TASK-433.6).
    /// cladefs: its changes counted in its cell's footprint on `host`.
    pub fn new_on(host: &Arc<crate::Host>, path: &Path) -> io::Result<Self> {
        Self::on_host(host, DiskDir::new_on(host, path)?, host.pages().clone())
    }

    /// The directory at `path` on the process's default host outside `madsim` ([`HostDir::new_on`]).
    #[cfg(not(madsim))]
    pub fn new(path: &Path) -> io::Result<Self> {
        Self::new_on(crate::Host::process(), path)
    }

    /// `dir` on `host`, its extents' files through `cache`, its changes counted in its cell's
    /// footprint.
    pub fn on_host(
        host: &Arc<crate::Host>,
        dir: DiskDir,
        cache: Arc<crate::pages::PageCache>,
    ) -> io::Result<Self> {
        let footprint = crate::footprint::of_host_on(host, &dir.path, &dir.disk)?;
        Self::over(crate::CachedDir::over(dir, cache), footprint)
    }

    /// `dir`, its extents' files through `cache`, its changes counted in its cell's footprint on
    /// `dir`'s host.
    pub fn on(dir: DiskDir, cache: Arc<crate::pages::PageCache>) -> io::Result<Self> {
        let host = Arc::clone(&dir.host);
        Self::on_host(&host, dir, cache)
    }

    /// The directory's path.
    pub fn path(&self) -> &Path {
        &self.under().under().path
    }
}

/// cladefs: a directory of the host's, its files bypassing the host's page cache: what a
/// [`HostDir`] holds its extents' files in a page cache of the process's over.
pub struct DiskDir {
    host: Arc<crate::Host>,
    path: PathBuf,
    /// The directory, open, for `barrier`.
    handle: Arc<host_fs::File>,
    /// The directory's device and inode, fixed for the life of `handle`.
    id: DirId,
    /// cladefs: the disk of its filesystem (doc-22).
    disk: Arc<bucket_cell::host_disk::HostDisk>,
    /// cladefs: its durability's deferral, if its maker defers it ([`crate::defer_durability`]).
    deferral: Option<Arc<crate::deferral::Deferral>>,
}

impl DiskDir {
    /// The directory at `path` on `host`, which exists.
    pub fn new_on(host: &Arc<crate::Host>, path: &Path) -> io::Result<Self> {
        use host_fs::MetadataExt as _;
        let handle = Arc::new(host_fs::File::open(path)?);
        let metadata = handle.metadata()?;
        let id = (metadata.dev(), metadata.ino());
        let disk = bucket_cell::host_disk::HostDisk::of_on(host.bucket(), path)?;
        let deferral = crate::deferral::of_on(host, path);
        Ok(Self { host: Arc::clone(host), path: path.to_owned(), handle, id, disk, deferral })
    }

    /// The directory at `path` on the process's default host outside `madsim`
    /// ([`DiskDir::new_on`]).
    #[cfg(not(madsim))]
    pub fn new(path: &Path) -> io::Result<Self> {
        Self::new_on(crate::Host::process(), path)
    }

    /// cladefs: a rename of a file in it, in flight until what is returned goes, if its
    /// durability is deferred: begun once no settling is, and waited for by the next
    /// ([`crate::defer_durability`]).
    fn renaming(&self) -> Option<crate::deferral::Renaming<'_>> {
        self.deferral.as_ref().map(|deferral| deferral.renaming())
    }

    /// cladefs: whether a sync of its, or of a file of its, is skipped now
    /// ([`crate::defer_durability`]).
    fn skips(deferral: &Option<Arc<crate::deferral::Deferral>>) -> bool {
        deferral.as_ref().is_some_and(|deferral| deferral.skips())
    }

    /// `file`, opened at `path`, bypassing the host's page cache, or refused, naming it, if its
    /// file system refuses that (`InvalidInput`, [`bucket_cell::direct::bypass`]).
    fn file(&self, path: &str, file: host_fs::File) -> io::Result<DiskFile> {
        bucket_cell::direct::bypass(&file, &self.path.join(path))?;
        Ok(DiskFile { file, tail: Mutex::default(), deferral: self.deferral.clone() })
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
        host_fs::remove_file(self.path.join(path))
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let _renaming = self.renaming();
        fail::fail_point!("dir::renaming");
        host_fs::rename(self.path.join(from), self.path.join(to))
    }

    fn link(&self, from: &str, to: &str) -> io::Result<()> {
        let _renaming = self.renaming();
        host_fs::hard_link(self.path.join(from), self.path.join(to))
    }

    fn entries(&self) -> io::Result<Vec<DirFile>> {
        use host_fs::MetadataExt as _;
        let mut found = Vec::new();
        let mut dirs = vec![String::new()];
        while let Some(dir) = dirs.pop() {
            for entry in host_fs::read_dir(self.path.join(&dir))? {
                let entry = entry?;
                let name = entry.file_name().into_string().map_err(|name| {
                    io::Error::new(io::ErrorKind::InvalidData, format!("{name:?} isn't UTF-8"))
                })?;
                let path = if dir.is_empty() { name } else { format!("{dir}/{name}") };
                let metadata = entry.metadata()?;
                if metadata.is_dir() {
                    dirs.push(path.clone());
                    found.push(DirFile { path, len: 0, allocated: 0, inode: 0, dir: true });
                } else {
                    let (len, inode) = (metadata.len(), metadata.ino());
                    let allocated = metadata.blocks() * 512;
                    found.push(DirFile { path, len, allocated, inode, dir: false });
                }
            }
        }
        Ok(found)
    }

    fn replace(&self, path: &str, bytes: &[u8]) -> io::Result<()> {
        let new = format!("{path}.new");
        // One a write that stopped left.
        match host_fs::remove_file(self.path.join(&new)) {
            Err(err) if !not_found(&err) => return Err(err),
            _ => {}
        }
        let file = self.create(&new, false)?;
        file.write_at(bytes, 0)?;
        file.sync()?;
        let _renaming = self.renaming();
        host_fs::rename(self.path.join(&new), self.path.join(path))
    }

    fn create_dir(&self, path: &str) -> io::Result<()> {
        host_fs::create_dir(self.path.join(path))
    }

    fn list(&self, path: &str) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in host_fs::read_dir(self.path.join(path))? {
            let name = entry?.file_name();
            names.push(name.into_string().map_err(|name| {
                io::Error::new(io::ErrorKind::InvalidData, format!("{name:?} isn't UTF-8"))
            })?);
        }
        Ok(names)
    }

    fn give(&self, path: &str, to: &mut dyn FnMut(&Path) -> io::Result<bool>) -> io::Result<bool> {
        to(&self.path.join(path))
    }

    fn sync_dir(&self, path: &str) -> io::Result<()> {
        if Self::skips(&self.deferral) {
            return Ok(());
        }
        let dir = host_fs::File::open(self.path.join(path))?;
        #[cfg(target_os = "macos")]
        {
            host_fs::sync_with(&dir, &[libc::F_BARRIERFSYNC, libc::F_FULLFSYNC])
        }
        #[cfg(not(target_os = "macos"))]
        {
            dir.sync_all()
        }
    }

    /// Takes the directory, counted, each `HostDir` of it taking it once: what is taken in this
    /// process another opening of it here shares, as a cell's uploader stopping, which holds it
    /// ([`LocalCell::hold_dir`](crate::LocalCell::hold_dir)), and its cell's next opening do;
    /// which cell is open here, its claim says ([`Dir::claim`], TASK-165.7). No other process's
    /// taking is waited for: none takes it (see [`Dir::take`]).
    fn take(&self) -> io::Result<()> {
        *self.host.dirs.taken.lock().unwrap().entry(self.id).or_default() += 1;
        Ok(())
    }

    fn let_go(&self) {
        let mut taken = self.host.dirs.taken.lock().unwrap();
        let Some(count) = taken.get_mut(&self.id) else { return };
        *count -= 1;
        if *count == 0 {
            taken.remove(&self.id);
        }
    }

    fn claim(&self) -> io::Result<()> {
        let mut claimed =
            closed_in(&self.host.dirs, self.host.dirs.claimed.lock().unwrap(), self.id)?;
        if claimed.contains_key(&self.id) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "this process has the cell open",
            ));
        }
        claimed.insert(self.id, false);
        Ok(())
    }

    fn closing(&self) {
        if let Some(closing) = self.host.dirs.claimed.lock().unwrap().get_mut(&self.id) {
            *closing = true;
        }
    }

    fn unclaim(&self) {
        self.host.dirs.claimed.lock().unwrap().remove(&self.id);
        self.host.dirs.unclaimed.notify_all();
        self.host.dirs.unclaimed_event.notify(usize::MAX);
    }

    fn barrier(&self) -> io::Result<()> {
        if Self::skips(&self.deferral) {
            return Ok(());
        }
        #[cfg(target_os = "macos")]
        {
            host_fs::sync_with(&self.handle, &[libc::F_FULLFSYNC])
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = &self.handle;
            Ok(())
        }
    }

    fn defers(&self) -> bool {
        Self::skips(&self.deferral)
    }

    fn made_deferred(&self) -> bool {
        self.deferral.as_ref().is_some_and(|deferral| deferral.made())
    }

    fn settle(&self) -> io::Result<()> {
        self.deferral.as_ref().map_or(Ok(()), |deferral| deferral.settle())
    }

    fn disk(&self) -> Option<&Arc<bucket_cell::host_disk::HostDisk>> {
        Some(&self.disk)
    }

    fn host(&self) -> Option<&Arc<crate::Host>> {
        Some(&self.host)
    }
}

/// cladefs: per-host table of taken directory locks and open/closing cell claims, owned by
/// [`crate::Host`].
pub(crate) struct Locks {
    /// The directories this host has taken ([`Dir::take`]), by their devices and inodes: how
    /// many takings of each.
    taken: Mutex<BTreeMap<DirId, usize>>,
    /// The directories whose cells this host has open ([`Dir::claim`]), and whether closing.
    claimed: Mutex<BTreeMap<DirId, bool>>,
    /// Woken as a claim goes ([`Dir::unclaim`]), for openings waiting for a cell closing.
    unclaimed: std::sync::Condvar,
    /// Said as a claim goes ([`Dir::unclaim`]), for async waits ([`closed_on`]).
    unclaimed_event: event_listener::Event,
}

impl Locks {
    pub(crate) fn new() -> Self {
        Self {
            taken: Mutex::new(BTreeMap::new()),
            claimed: Mutex::new(BTreeMap::new()),
            unclaimed: std::sync::Condvar::new(),
            unclaimed_event: event_listener::Event::new(),
        }
    }
}

/// cladefs: `claimed`, `locks.claimed` locked, once no cell of the directory `id` is closing on
/// this host, which it waits for a while ([`CLOSING_WAIT`]): `WouldBlock` if one still is.
fn closed_in<'a>(
    locks: &'a Locks,
    mut claimed: std::sync::MutexGuard<'a, BTreeMap<DirId, bool>>,
    id: DirId,
) -> io::Result<std::sync::MutexGuard<'a, BTreeMap<DirId, bool>>> {
    let deadline = std::time::Instant::now() + CLOSING_WAIT;
    while claimed.get(&id) == Some(&true) {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "this process is still closing the cell",
            ));
        }
        fail::fail_point!("dir::claim_waits");
        // cladefs: never under madsim, where an opening of a cell waits for its close first,
        // holding no thread ([`closed_on`]), then claims it, awaiting nothing between (doc-24,
        // TASK-685.3). The wait stays for what claims from a thread of its own, an outermost
        // entry's (board 2946): the sync openings, `LocalCell::recover` and what opens over it,
        // which tests and tools call as they are, have no `closed` to await first; and off madsim
        // a cell another thread begins closing between an opening's `closed` and its claim is
        // waited for here, rather than refused as one open.
        crate::blocks!("an opening, waiting for a cell of its directory closing in the process");
        claimed = locks.unclaimed.wait_timeout(claimed, left).unwrap().0;
    }
    Ok(claimed)
}

/// cladefs: returns once no cell of the directory at `path` is closing on `host`, waiting
/// for one that is a while, as an opening of it does ([`Dir::claim`]), holding no thread:
/// `WouldBlock` if it still is. What reads in the bucket what a cell's close uploads, a fork of a
/// version its closing cell made, say, waits for the close so (TASK-644); and an opening of the
/// cell, first, so that its claim finds none closing (doc-24). Nothing at `path`, nothing closes
/// there.
pub async fn closed_on(host: &crate::Host, path: &Path) -> io::Result<()> {
    use host_fs::MetadataExt as _;
    let metadata = match crate::upload::blocking(|| host_fs::metadata(path)) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    let id = (metadata.dev(), metadata.ino());
    let closing = async {
        loop {
            let unclaimed = host.dirs.unclaimed_event.listen();
            if host.dirs.claimed.lock().unwrap().get(&id) != Some(&true) {
                return;
            }
            fail::fail_point!("dir::claim_waits");
            unclaimed.await;
        }
    };
    tokio::time::timeout(CLOSING_WAIT, closing).await.map_err(|_| {
        io::Error::new(io::ErrorKind::WouldBlock, "this process is still closing the cell")
    })
}

/// cladefs: returns once no cell of the directory at `path` is claimed on `host` (neither open nor
/// closing), waiting up to [`CLOSING_WAIT`] for any in-flight close to release its directory claim
/// (board 11530): closes the window between `Arc::into_inner` dropping the handle and
/// `LocalCell::close` calling `closing()`. Nothing at `path`, nothing claimed there.
pub async fn unclaimed_on(host: &crate::Host, path: &Path) -> io::Result<()> {
    use host_fs::MetadataExt as _;
    let metadata = match crate::upload::blocking(|| host_fs::metadata(path)) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    let id = (metadata.dev(), metadata.ino());
    let unclaimed = async {
        loop {
            let notified = host.dirs.unclaimed_event.listen();
            if !host.dirs.claimed.lock().unwrap().contains_key(&id) {
                return;
            }
            fail::fail_point!("dir::claim_waits");
            notified.await;
        }
    };
    tokio::time::timeout(CLOSING_WAIT, unclaimed)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "this process still holds the cell"))
}

/// cladefs: [`closed_on`] on the process's default host outside `madsim`.
#[cfg(not(madsim))]
pub async fn closed(path: &Path) -> io::Result<()> {
    closed_on(crate::Host::process(), path).await
}

/// cladefs: how long an opening waits for a cell of its directory closing in this process
/// (TASK-648): a close makes its last commits durable, seconds on a busy disk.
const CLOSING_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// A directory's device and inode, which name it however a path names it.
type DirId = (u64, u64);

/// cladefs: a file of the host's disk, bypassing its page cache ([`DiskDir`]): read and written by
/// the blocks around what is asked, from memory aligned to them, the block ending the last write
/// held, so that an append, a log's, reads nothing back (`bucket_cell::direct`, TASK-433).
pub struct DiskFile {
    file: host_fs::File,
    tail: Mutex<bucket_cell::direct::Tail>,
    /// cladefs: its directory's durability's deferral, if its maker defers it
    /// ([`crate::defer_durability`]).
    deferral: Option<Arc<crate::deferral::Deferral>>,
}

impl HostFile {
    /// cladefs: the host's file under the page cache and the counting: for tests that refuse its
    /// descriptor's system calls.
    pub fn host_file(&self) -> &host_fs::File {
        &self.under().under().file
    }
}

impl File for DiskFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        bucket_cell::direct::read_at(&self.file, buf, offset)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        let mut tail = self.tail.lock().unwrap();
        bucket_cell::direct::write_at(&self.file, &mut tail, buf, offset)
    }

    /// Taken as no write is in flight: a write of part of a block writes it whole, the file cut
    /// back after, which a size taken between would see past the file's end.
    fn size(&self) -> io::Result<u64> {
        let _writes = self.tail.lock().unwrap();
        Ok(self.file.metadata()?.len())
    }

    fn sync(&self) -> io::Result<()> {
        if DiskDir::skips(&self.deferral) {
            return Ok(());
        }
        #[cfg(target_os = "macos")]
        {
            host_fs::sync_with(&self.file, &[libc::F_BARRIERFSYNC, libc::F_FULLFSYNC])
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.file.sync_data()
        }
    }

    /// Held as no write is in flight, as [`DiskFile::size`] is: a write of part of a block writes
    /// it whole.
    fn punch(&self, offset: u64, len: u64) -> io::Result<()> {
        let _writes = self.tail.lock().unwrap();
        host_fs::punch_hole(&self.file, offset, len)
    }

    fn allocated(&self) -> io::Result<u64> {
        use host_fs::MetadataExt as _;
        let _writes = self.tail.lock().unwrap();
        Ok(self.file.metadata()?.blocks() * 512)
    }
}
