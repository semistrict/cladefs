// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A cell directory in memory, which simulates crashes, for tests.
//!
//! Its files and directories are a file system in memory, [`host_fs::mem`], which each simulated
//! host's disk is too, so that the cells' crash tests and the simulation crash alike (board 8264):
//! it keeps what a host's file system would have made durable, a file's contents as of its last
//! sync and a directory's entries as of its last sync, and a crash ([`MemDisk::crash`]) keeps that,
//! and any of what happened since, at random: each sector of each write since the file's last sync,
//! and each entry made, removed or renamed since its directory's last sync, in order. A directory
//! made is durable at the next sync of anything, which commits a journaling file system's journal,
//! as ext4 and XFS have it (board 5748); a crash before loses it, or keeps it, at random, with all
//! under it. A crash may also corrupt what it kept of the writes since
//! ([`MemDisk::crash_corrupting`], TASK-680, as FoundationDB's simulation's power losses do): each
//! sector written since its file's last sync there whole, not at all, garbage, or torn, at a random
//! offset ([`host_fs::crash`]).
//!
//! What it checks besides is a cell's: a rename or link refuses to go over a file there
//! ([`Dir::rename`]); its disk may be of a capacity ([`MemDisk::with_capacity`]); and
//! [`MemDir::fail_after`] makes the file system fail every operation that would change it after a
//! number of them, as a crash would interrupt them. cladefs: which of them fails, and so what a
//! crash keeps, is a function of the cell's calls, and of the crash's seed, only if the cell does
//! no I/O on threads of its own, in a directory made [`MemDir::serial`], or if a seeded schedule
//! orders that I/O, as Shuttle's do; it says whether the first that failed was a thread's of the
//! cell's ([`MemDir::failed_elsewhere`], TASK-79.79).

use crate::cached::{CachedDir, CachedFile};
use crate::dir::{Dir, DirFile, File};
use crate::pages::{Options, PageCache};
use crate::sync::{Mutex, thread};
use bucket_cell::host_disk::{self, HostDisk, Sample};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A cell directory in memory, under its page cache ([`MemDir`]). See the module docs.
#[derive(Clone)]
pub struct MemDisk {
    state: Arc<Mutex<State>>,
    /// cladefs: its disk, of its capacity ([`MemDisk::with_capacity`], doc-22).
    disk: Arc<HostDisk>,
    /// cladefs: the footprint its cell's files are counted in ([`crate::footprint`]).
    footprint: Arc<crate::footprint::Footprint>,
    /// cladefs: whether the cell does all of its I/O in it on its callers' threads
    /// ([`MemDir::serial`]): as it never changes, asking takes no lock ([`Dir::concurrent`]), which
    /// under Shuttle would be a scheduling point of its own.
    serial: bool,
}

/// A file of a [`MemDisk`].
pub struct MemDiskFile {
    state: Arc<Mutex<State>>,
    inode: u64,
}

#[derive(Clone, Default)]
struct State {
    /// Its files and directories, and what a crash would leave of them.
    fs: host_fs::mem::Fs,
    /// cladefs: the bytes written to each file since the last write-back of any of its range, or
    /// sync, by its inode.
    unwritten: HashMap<u64, u64>,
    /// How many more operations that change the file system succeed, if limited.
    budget: Option<u64>,
    /// cladefs: the thread that last set the budget ([`MemDir::fail_after`]), if any.
    limited_on: Option<thread::ThreadId>,
    /// cladefs: the thread whose operation the file system failed first since the budget was last
    /// set, if any.
    failed_on: Option<thread::ThreadId>,
    /// The bytes read and written since it was made, or crashed, and the files opened.
    io: Io,
    /// cladefs: the most it holds, if limited ([`MemDisk::with_capacity`]).
    capacity: Option<u64>,
}

/// The bytes a [`MemDir`]'s files were read and written; cladefs: and how many times its files were
/// opened (TASK-92.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Io {
    pub read: u64,
    pub written: u64,
    pub opened: u64,
    /// cladefs: the most bytes a file held written, but neither written back ([`File::write_back`])
    /// nor synced since, at any time: what a sync of it would have had to write (TASK-165.9).
    pub most_unwritten: u64,
    /// cladefs: the most bytes a sync of a file found written, but neither written back nor synced
    /// since: what it wrote at once (TASK-401).
    pub most_synced_unwritten: u64,
    /// cladefs: the syncs of directories, and of files (TASK-369, TASK-333).
    pub synced_dirs: u64,
    pub synced_files: u64,
}

/// `err`, of a call on `path`: one saying nothing is there names it, as a cell's errors then say
/// which of its files is missing.
fn at(path: &str) -> impl FnOnce(io::Error) -> io::Error + '_ {
    move |err| match err.kind() {
        io::ErrorKind::NotFound => {
            io::Error::new(io::ErrorKind::NotFound, format!("{path} doesn't exist"))
        }
        _ => err,
    }
}

/// `path`, of names made of `&str`s, as one.
fn utf8(path: PathBuf) -> String {
    path.to_string_lossy().into_owned()
}

impl State {
    /// Spends one of the operations the budget allows, failing if none are left.
    fn spend(&mut self) -> io::Result<()> {
        match &mut self.budget {
            Some(0) => {
                self.failed_on.get_or_insert_with(|| thread::current().id());
                Err(io::Error::other("the file system crashed"))
            }
            Some(budget) => {
                *budget -= 1;
                Ok(())
            }
            None => Ok(()),
        }
    }

    fn lookup(&self, path: &str) -> io::Result<u64> {
        self.fs.lookup(Path::new(path)).map_err(at(path))
    }

    /// cladefs: what it holds, as a disk counts it ([`host_disk::counted`]): each file reachable
    /// once, however many names it has, and each directory but its root a granule.
    fn used(&self) -> u64 {
        self.fs.used(host_disk::counted, host_disk::GRANULE)
    }

    /// cladefs: `ENOSPC` unless `more` bytes, counted, fit in its capacity, as a full disk refuses
    /// what would pass it.
    fn fits(&self, more: u64) -> io::Result<()> {
        match self.capacity {
            Some(capacity) if self.used() + more > capacity => {
                Err(io::Error::from_raw_os_error(libc::ENOSPC))
            }
            _ => Ok(()),
        }
    }

    /// `AlreadyExists` unless nothing is at `path`, as a cell's rename asks ([`Dir::rename`]): one
    /// going over a file there is the cell's mistake, which a host's rename would hide.
    fn vacant(&self, path: &str) -> io::Result<()> {
        match self.fs.lookup(Path::new(path)) {
            Ok(_) => Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{path} exists"))),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }
}

impl MemDisk {
    /// An empty directory.
    pub fn new() -> Self {
        Self::over(State::default(), false)
    }

    /// cladefs: an empty directory on a disk of its own of `capacity` bytes, which refuses with
    /// `ENOSPC` what would pass it, as a full filesystem does: its writes, and its makings of
    /// files and directories (doc-22).
    pub fn with_capacity(capacity: u64) -> Self {
        Self::over(State { capacity: Some(capacity), ..State::default() }, false)
    }

    /// The directory holding `state`, on a disk of its own that says what it holds.
    fn over(state: State, serial: bool) -> Self {
        let state = Arc::new(Mutex::new(state));
        let held = Arc::downgrade(&state);
        let disk = HostDisk::in_memory(move || {
            let Some(state) = held.upgrade() else { return Sample::default() };
            let state = state.lock().unwrap();
            let (size, used) = (state.capacity.unwrap_or(u64::MAX), state.used());
            Sample { size, free: size.saturating_sub(used), used }
        });
        let footprint = crate::footprint::Footprint::new(None, Some(disk.clone()));
        Self { state, disk, footprint, serial }
    }

    /// cladefs: an empty directory in which the cell does all of its I/O on its callers' threads,
    /// none on threads of its own ([`Dir::concurrent`]): so that its operations come in an order
    /// the cell's calls alone decide, and so where it fails ([`MemDir::fail_after`]), and what a
    /// crash of a seed keeps; as do the directories its crashes leave (TASK-79.79).
    pub fn serial() -> Self {
        Self { serial: true, ..Self::new() }
    }

    /// cladefs: its disk ([`MemDisk::with_capacity`]).
    pub fn disk(&self) -> &Arc<HostDisk> {
        &self.disk
    }

    /// Makes every operation that changes the file system fail after `operations` more of them,
    /// or none if `None`.
    pub fn fail_after(&self, operations: Option<u64>) {
        let mut state = self.state.lock().unwrap();
        state.budget = operations;
        (state.limited_on, state.failed_on) = (Some(thread::current().id()), None);
    }

    /// The bytes its files were read and written so far.
    pub fn io(&self) -> Io {
        self.state.lock().unwrap().io
    }

    /// Whether the file system failed an operation, or would fail the next, having spent its
    /// budget (see `fail_after`).
    pub fn has_failed(&self) -> bool {
        self.state.lock().unwrap().budget == Some(0)
    }

    /// cladefs: whether the first operation the file system failed, since its budget was last set
    /// ([`MemDir::fail_after`]), was another thread's than the one that set it: in a test whose
    /// thread runs the cell, and sets the budget, one of the cell's own, framing a segment or
    /// syncing a flush's files, which a crash then interrupted (TASK-79.79).
    pub fn failed_elsewhere(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.failed_on.is_some() && state.failed_on != state.limited_on
    }

    /// cladefs: every file in it, by its path, with what it holds now, durable or not: what tests
    /// search for bytes that must be in no file of a host's (TASK-411.9).
    pub fn files(&self) -> Vec<(String, Vec<u8>)> {
        let state = self.state.lock().unwrap();
        let files = state.fs.walk().into_iter().filter(|(_, stat)| !stat.dir);
        files
            .map(|(path, stat)| {
                let data = state.fs.data(stat.inode).expect("a file walked to holds bytes");
                (utf8(path), data.to_vec())
            })
            .collect()
    }

    /// cladefs: the directory as a process killed with its host up leaves it (board 5744): all it
    /// wrote, durable or not, its writes since their syncs at a later power loss's mercy still,
    /// which [`MemDisk::crash`] of what this returns models.
    pub fn killed(&self) -> MemDisk {
        let mut state = self.state.lock().unwrap().clone();
        (state.budget, state.limited_on, state.failed_on) = (None, None, None);
        Self::over(state, self.serial)
    }

    /// The directory as a crash would leave it: what was durable, and any of what happened since,
    /// chosen at random by an RNG seeded with `seed`. See the module docs.
    pub fn crash(&self, seed: u64) -> MemDisk {
        self.crash_with(seed, false)
    }

    /// cladefs: the directory as a crash would leave it, as [`MemDisk::crash`] says, each block
    /// written since its file's last sync also garbage, or torn, at random (TASK-680): garbage in
    /// the bytes its write changes alone, as powersafe overwrite has it (board 5890, decision-3).
    pub fn crash_corrupting(&self, seed: u64) -> MemDisk {
        self.crash_with(seed, true)
    }

    fn crash_with(&self, seed: u64, corrupting: bool) -> MemDisk {
        let state = self.state.lock().unwrap();
        let fs = state.fs.crashed(seed, corrupting);
        MemDisk::over(State { fs, capacity: state.capacity, ..State::default() }, self.serial)
    }
}

/// A cell directory in memory, which simulates crashes ([`MemDisk`]), its extents' files through
/// a page cache of its own, as a host's are through the process's ([`crate::HostDir`], TASK-433):
/// small, so that the cell's tests write back often, and with no flusher, so that what is written
/// when is a function of the cell's calls alone ([`PageCache::without_flusher`]). What its pages
/// hold dirty a crash loses, as a host's crash loses a process's memory.
///
/// cladefs: its changes counted, as it makes them, in its footprint ([`crate::Metered`],
/// TASK-539.4), as a host's are.
pub type MemDir = crate::Metered<CachedDir<MemDisk>>;

/// A file of a [`MemDir`].
pub type MemFile = crate::MeteredFile<CachedFile<MemDiskFile>>;

/// The page cache of a [`MemDir`].
fn page_cache() -> std::sync::Arc<PageCache> {
    PageCache::without_flusher(Options {
        background: 256 << 10,
        hard: 1 << 20,
        most: 4 << 20,
        ..Options::default()
    })
}

impl MemDir {
    /// An empty directory ([`MemDisk::new`]).
    pub fn new() -> Self {
        Self::on(MemDisk::new(), page_cache())
    }

    /// An empty directory whose page cache goes by `options`, with no flusher: what a test of how
    /// the cell paces its own write-back holds dirty under a cache as large as a host's.
    pub fn with_cache(options: Options) -> Self {
        Self::on(MemDisk::new(), PageCache::without_flusher(options))
    }

    /// cladefs: an empty directory in which the cell does all of its I/O on its callers' threads
    /// ([`MemDisk::serial`]).
    pub fn serial() -> Self {
        Self::on(MemDisk::serial(), page_cache())
    }

    /// cladefs: an empty directory on a disk of `capacity` bytes ([`MemDisk::with_capacity`]).
    pub fn with_capacity(capacity: u64) -> Self {
        Self::on(MemDisk::with_capacity(capacity), page_cache())
    }

    /// cladefs: `disk`, its extents' files through `cache`, its changes counted in its footprint.
    pub fn on(disk: MemDisk, cache: std::sync::Arc<PageCache>) -> Self {
        let footprint = disk.footprint.clone();
        Self::over(CachedDir::over(disk, cache), footprint).expect("a directory in memory lists")
    }

    /// The directory in memory under its page cache.
    pub fn disk(&self) -> &MemDisk {
        self.under().under()
    }

    /// As [`MemDisk::fail_after`].
    pub fn fail_after(&self, operations: Option<u64>) {
        self.disk().fail_after(operations)
    }

    /// As [`MemDisk::io`]: what reached the directory under the page cache.
    pub fn io(&self) -> Io {
        self.disk().io()
    }

    /// As [`MemDisk::has_failed`].
    pub fn has_failed(&self) -> bool {
        self.disk().has_failed()
    }

    /// As [`MemDisk::failed_elsewhere`].
    pub fn failed_elsewhere(&self) -> bool {
        self.disk().failed_elsewhere()
    }

    /// cladefs: every file in it, by its path, with what it holds now ([`MemDisk::files`]), what
    /// its page cache holds dirty written back first, as it would be: what tests search for bytes
    /// that must be in no file of a host's (TASK-411.9).
    pub fn files(&self) -> Vec<(String, Vec<u8>)> {
        if let Err(err) = self.cache().write_back_all() {
            log::warn!("extent store: writing back a directory in memory: {err}");
        }
        self.disk().files()
    }

    /// cladefs: the directory as a process killed with its host up leaves it ([`MemDisk::killed`]):
    /// what its page cache held dirty lost, as a process's memory is.
    pub fn killed(&self) -> MemDir {
        Self::on(self.disk().killed(), page_cache())
    }

    /// The directory as a crash would leave it ([`MemDisk::crash`]): what its page cache held
    /// dirty lost, as a process's memory is; its footprint counted anew, as listed.
    pub fn crash(&self, seed: u64) -> MemDir {
        Self::on(self.disk().crash(seed), page_cache())
    }

    /// cladefs: the directory as a crash would leave it, as [`MemDir::crash`] says, what it kept
    /// of the writes since their files' last syncs corrupted at random
    /// ([`MemDisk::crash_corrupting`], TASK-680).
    pub fn crash_corrupting(&self, seed: u64) -> MemDir {
        Self::on(self.disk().crash_corrupting(seed), page_cache())
    }
}

impl Dir for MemDisk {
    type File = MemDiskFile;

    fn create(&self, path: &str, _data: bool) -> io::Result<MemDiskFile> {
        let mut state = self.state.lock().unwrap();
        // A full disk makes no file.
        state.fits(1)?;
        state.spend()?;
        let inode = state.fs.create(Path::new(path)).map_err(at(path))?;
        Ok(MemDiskFile { state: self.state.clone(), inode })
    }

    fn open(&self, path: &str, _data: bool) -> io::Result<MemDiskFile> {
        let mut state = self.state.lock().unwrap();
        let inode = state.lookup(path)?;
        if state.fs.stat(inode)?.dir {
            return Err(io::Error::from_raw_os_error(libc::EISDIR));
        }
        state.io.opened += 1;
        Ok(MemDiskFile { state: self.state.clone(), inode })
    }

    fn remove(&self, path: &str) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        state.spend()?;
        state.fs.remove(Path::new(path), false).map_err(at(path))
    }

    /// cladefs: within a directory, at once; from one to another, its removal from the one and its
    /// entry in the other each durable as its directory is synced (TASK-310). Never over a file
    /// there.
    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        state.spend()?;
        state.lookup(from)?;
        state.vacant(to)?;
        state.fs.rename(Path::new(from), Path::new(to)).map_err(at(to))
    }

    /// cladefs: `to`'s entry, of `from`'s file, durable as its directory is synced (TASK-310).
    fn link(&self, from: &str, to: &str) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        state.spend()?;
        state.lookup(from)?;
        state.fs.link(Path::new(from), Path::new(to)).map_err(at(to))
    }

    /// cladefs: its file written and synced, then put at `path`, which a crash keeps or not until
    /// its directory is synced.
    fn replace(&self, path: &str, bytes: &[u8]) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        state.fits(host_disk::counted(bytes.len() as u64))?;
        state.spend()?;
        state.fs.replace(Path::new(path), bytes).map_err(at(path))?;
        state.io.written += bytes.len() as u64;
        state.io.synced_files += 1;
        Ok(())
    }

    fn create_dir(&self, path: &str) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        state.fits(host_disk::GRANULE)?;
        state.spend()?;
        state.fs.create_dir(Path::new(path)).map_err(at(path))
    }

    /// Its files' and directories' names, in order.
    fn list(&self, path: &str) -> io::Result<Vec<String>> {
        let state = self.state.lock().unwrap();
        let listed = state.fs.list(Path::new(path)).map_err(at(path))?;
        Ok(listed.into_iter().map(|(name, _)| utf8(name.into())).collect())
    }

    fn entries(&self) -> io::Result<Vec<DirFile>> {
        let state = self.state.lock().unwrap();
        let found = state.fs.walk().into_iter().map(|(path, stat)| match stat.dir {
            true => DirFile { path: utf8(path), len: 0, allocated: 0, inode: 0, dir: true },
            false => DirFile {
                path: utf8(path),
                len: stat.len,
                allocated: stat.allocated,
                inode: stat.inode,
                dir: false,
            },
        });
        Ok(found.collect())
    }

    fn sync_dir(&self, path: &str) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        state.spend()?;
        let inode = state.lookup(path)?;
        if !state.fs.stat(inode)?.dir {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        state.fs.sync(inode)?;
        state.io.synced_dirs += 1;
        Ok(())
    }

    fn barrier(&self) -> io::Result<()> {
        self.state.lock().unwrap().spend()
    }

    fn concurrent(&self) -> bool {
        !self.serial
    }

    fn disk(&self) -> Option<&Arc<HostDisk>> {
        Some(&self.disk)
    }
}

impl File for MemDiskFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        state.io.read += buf.len() as u64;
        let data = state.fs.data(self.inode)?;
        let offset = (offset as usize).min(data.len());
        let held = (data.len() - offset).min(buf.len());
        buf[..held].copy_from_slice(&data[offset..offset + held]);
        buf[held..].fill(0);
        Ok(())
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        let len = state.fs.data(self.inode)?.len() as u64;
        let end = (offset + buf.len() as u64).max(len);
        state.fits(host_disk::counted(end) - host_disk::counted(len))?;
        state.spend()?;
        state.fs.write(self.inode, offset, buf)?;
        state.io.written += buf.len() as u64;
        let unwritten = state.unwritten.entry(self.inode).or_default();
        *unwritten += buf.len() as u64;
        let unwritten = *unwritten;
        state.io.most_unwritten = state.io.most_unwritten.max(unwritten);
        Ok(())
    }

    fn write_back(&self, _offset: u64, _len: u64) -> io::Result<()> {
        self.state.lock().unwrap().unwritten.remove(&self.inode);
        Ok(())
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.state.lock().unwrap().fs.data(self.inode)?.len() as u64)
    }

    fn sync(&self) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        state.spend()?;
        state.fs.sync(self.inode)?;
        let unwritten = state.unwritten.remove(&self.inode).unwrap_or(0);
        state.io.synced_files += 1;
        state.io.most_synced_unwritten = state.io.most_synced_unwritten.max(unwritten);
        Ok(())
    }

    /// Written as zeros, as a crash keeps or loses a write, and its whole blocks given back.
    fn punch(&self, offset: u64, len: u64) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        state.spend()?;
        state.fs.punch(self.inode, offset, len)
    }

    fn allocated(&self) -> io::Result<u64> {
        Ok(self.state.lock().unwrap().fs.stat(self.inode)?.allocated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(file: &MemDiskFile) -> Vec<u8> {
        let mut data = vec![0; file.size().unwrap() as usize];
        file.read_at(&mut data, 0).unwrap();
        data
    }

    /// Creating a file where one is fails, as a host's `create_new` does, leaving the one there as
    /// it was, and those made after it their own.
    #[test]
    fn creating_where_a_file_is_leaves_it() {
        let dir = MemDisk::new();
        dir.create("f", false).unwrap().write_at(b"a", 0).unwrap();
        let error = dir.create("f", false).err().expect("created over a file");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        dir.create("g", false).unwrap().write_at(b"b", 0).unwrap();
        let read_at = |path| read(&dir.open(path, false).unwrap());
        assert_eq!((read_at("f"), read_at("g")), (b"a".to_vec(), b"b".to_vec()));
    }

    #[test]
    fn fails_after_its_budget() {
        let dir = MemDisk::new();
        let file = dir.create("f", false).unwrap();
        dir.fail_after(Some(2));
        file.write_at(b"a", 0).unwrap();
        file.sync().unwrap();
        assert!(file.write_at(b"b", 1).is_err());
        assert!(dir.create("g", false).is_err());
        assert!(dir.has_failed());
        // Reads still work.
        assert_eq!(read(&file), b"a");
    }

    /// cladefs: a directory of a capacity refuses with `ENOSPC` the writes, files and directories
    /// that would pass it, changing nothing, and its disk says what it holds, each file counted in
    /// granules once however many names it has (doc-22).
    #[test]
    fn a_capacity_refuses_what_would_pass_it() {
        const G: u64 = host_disk::GRANULE;
        let full =
            |result: io::Result<()>| result.unwrap_err().raw_os_error() == Some(libc::ENOSPC);
        let dir = MemDisk::with_capacity(4 * G);
        let sample = || dir.disk().sample().unwrap();
        assert_eq!(sample(), Sample { size: 4 * G, free: 4 * G, used: 0 });
        dir.create_dir("d").unwrap();
        let file = dir.create("d/f", true).unwrap();
        file.write_at(&[1; 100], 0).unwrap();
        dir.link("d/f", "d/g").unwrap();
        assert_eq!(sample(), Sample { size: 4 * G, free: 2 * G, used: 2 * G });
        // Past it by a byte, the write is refused whole; within its last granule, not.
        assert!(full(file.write_at(&[2; 1], 3 * G)));
        assert_eq!(file.size().unwrap(), 100);
        file.write_at(&[3; 1], 3 * G - 1).unwrap();
        assert_eq!(sample().free, 0);
        // Full, it makes nothing, but rewrites what it holds.
        assert!(full(dir.create("d/h", false).map(drop)));
        assert!(full(dir.create_dir("e")));
        file.write_at(&[4; 100], 0).unwrap();
        // What goes frees its room.
        dir.remove("d/g").unwrap();
        assert_eq!(sample().free, 0, "still at a name");
        dir.remove("d/f").unwrap();
        assert_eq!(sample(), Sample { size: 4 * G, free: 3 * G, used: G });
        // A crash keeps its capacity.
        assert_eq!(dir.crash(0).disk().sample().unwrap().size, 4 * G);
    }

    /// TASK-539.3: a file replaced is there whole, its old one or its new, after any crash, and
    /// the new one durably once it returns; one stopped part way, as a crash stops it, leaves
    /// either. A rename still refuses to go over a file there.
    #[test]
    fn replacing_leaves_the_old_file_or_the_new() {
        let dir = MemDisk::new();
        dir.replace("r", b"old").unwrap();
        dir.replace("r", b"new").unwrap();
        dir.sync_dir("").unwrap();
        for seed in 0..16 {
            assert_eq!(read(&dir.crash(seed).open("r", false).unwrap()), b"new", "seed {seed}");
        }
        // Its directory not synced after: either.
        dir.replace("r", b"newer").unwrap();
        dir.fail_after(Some(0));
        assert!(dir.replace("r", b"newest").is_err(), "failing, it changes nothing");
        assert_eq!(read(&dir.open("r", false).unwrap()), b"newer");
        let mut seen = std::collections::BTreeSet::new();
        for seed in 0..64 {
            seen.insert(read(&dir.crash(seed).open("r", false).unwrap()));
        }
        assert_eq!(seen, [b"new".to_vec(), b"newer".to_vec()].into());
        dir.fail_after(None);
        dir.create("s", false).unwrap();
        let refused = dir.rename("s", "r").unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::AlreadyExists);
    }

    /// cladefs: it says whether the first operation it failed, since its budget was set, was
    /// another thread's than the one that set it (TASK-79.79).
    #[test]
    fn says_whose_operation_failed_first() {
        let dir = MemDisk::new();
        // Files there, so that removing one fails only as the file system crashed.
        for name in ["f", "g"] {
            dir.create(name, false).unwrap();
        }
        // What removing one said, and what it says as the file system crashes.
        let said = |removed: io::Result<()>| removed.map_err(|err| err.to_string());
        let crashed = Err("the file system crashed".to_owned());
        let elsewhere =
            |name| std::thread::scope(|scope| scope.spawn(|| dir.remove(name)).join().unwrap());
        dir.fail_after(Some(0));
        assert!(!dir.failed_elsewhere(), "nothing failed");
        assert_eq!(said(elsewhere("f")), crashed);
        assert_eq!(said(dir.remove("g")), crashed);
        assert!(dir.failed_elsewhere());
        dir.fail_after(Some(0));
        assert_eq!(said(dir.remove("f")), crashed);
        assert_eq!(said(elsewhere("g")), crashed);
        assert!(!dir.failed_elsewhere());
    }
}
