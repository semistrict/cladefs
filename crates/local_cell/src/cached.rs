// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a store's directory with its data files through a page cache of the process's
//! ([`crate::pages`], TASK-433): what every directory a store opens is, the host's
//! ([`crate::HostDir`]) and one in memory ([`crate::MemDir`]) alike, so that no store, a test's
//! no more than a host's, runs around it.
//!
//! Its extents' files (`data`) are held in the cache, one set of pages a file however many times
//! and by whatever names it is opened: any opening of a name whose file is cached shares its
//! pages, however it is opened, so that what of it is dirty is nowhere else. Its other files, logs
//! and descriptions, written a little at a time and read back seldom, go to the directory's own
//! as they are. A file removed, or replaced by a rename, is unlinked, unless it is at another name
//! still: its openings go on with its pages, and the last drops them, the dirty ones unwritten.

use crate::dir::{Dir, File};
use crate::pages::{Cached, PageCache, WeakCached};
use crate::sync::Mutex;
use std::collections::HashMap;
use std::io;
use std::ops::Range;
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::sync::Arc;

/// A directory, `D`, its extents' files through a page cache. See the module docs.
pub struct CachedDir<D: Dir> {
    shared: Arc<Shared<D>>,
}

struct Shared<D: Dir> {
    dir: D,
    cache: Arc<PageCache>,
    /// Its extents' files open, by name, each with its pages; a file's pages go with its last
    /// opening, its dirty ones written back.
    files: Mutex<HashMap<String, WeakCached<D::File>>>,
}

/// Its clones are the same directory: they share its files' pages.
impl<D: Dir> Clone for CachedDir<D> {
    fn clone(&self) -> Self {
        Self { shared: self.shared.clone() }
    }
}

impl<D: Dir> CachedDir<D> {
    /// `dir`, its extents' files through `cache`.
    pub fn over(dir: D, cache: Arc<PageCache>) -> Self {
        Self { shared: Arc::new(Shared { dir, cache, files: Mutex::default() }) }
    }

    /// The directory under the cache.
    pub fn under(&self) -> &D {
        &self.shared.dir
    }

    /// The cache its files' pages are in.
    pub fn cache(&self) -> &Arc<PageCache> {
        &self.shared.cache
    }

    /// The opening of the file at `path`, if it is cached and open.
    fn cached(&self, path: &str) -> Option<Cached<D::File>> {
        self.shared.files.lock().unwrap().get(path).and_then(WeakCached::upgrade)
    }

    /// `file`, opened at `path`, as the store's: through the cache if it is an extents' file, or
    /// if an opening of it is.
    fn file(&self, path: &str, file: D::File, data: bool) -> io::Result<CachedFile<D::File>> {
        let mut files = self.shared.files.lock().unwrap();
        files.retain(|_, file| file.upgrade().is_some());
        if let Some(open) = files.get(path).and_then(WeakCached::upgrade) {
            return Ok(CachedFile::Cached(open));
        }
        if !data {
            return Ok(CachedFile::Raw(file));
        }
        let opened = Cached::new(file, &self.shared.cache)?;
        files.insert(path.to_owned(), opened.downgrade());
        Ok(CachedFile::Cached(opened))
    }

    /// `gone`, a file no longer at a name of `files`, unlinked unless it is still at another: its
    /// last opening drops its pages unwritten.
    fn forget(files: &HashMap<String, WeakCached<D::File>>, gone: Option<WeakCached<D::File>>) {
        let Some(gone) = gone.as_ref().and_then(WeakCached::upgrade) else { return };
        if !files.values().filter_map(WeakCached::upgrade).any(|other| other.same(&gone)) {
            gone.unlink();
        }
    }
}

impl<D: Dir> Dir for CachedDir<D> {
    type File = CachedFile<D::File>;

    /// A file made anew at a name holds nothing of one there before.
    fn create(&self, path: &str, data: bool) -> io::Result<Self::File> {
        let file = self.shared.dir.create(path, data)?;
        {
            let mut files = self.shared.files.lock().unwrap();
            let gone = files.remove(path);
            Self::forget(&files, gone);
        }
        self.file(path, file, data)
    }

    fn open(&self, path: &str, data: bool) -> io::Result<Self::File> {
        let file = self.shared.dir.open(path, data)?;
        self.file(path, file, data)
    }

    /// And unlinks its pages, unless it is at another name still: its last opening drops them.
    fn remove(&self, path: &str) -> io::Result<()> {
        self.shared.dir.remove(path)?;
        let mut files = self.shared.files.lock().unwrap();
        let gone = files.remove(path);
        Self::forget(&files, gone);
        Ok(())
    }

    /// A file it replaces is unlinked, unless it is at another name still.
    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.shared.dir.rename(from, to)?;
        let mut files = self.shared.files.lock().unwrap();
        let moved = files.remove(from);
        let replaced = files.remove(to);
        if let Some(moved) = moved {
            files.insert(to.to_owned(), moved);
        }
        Self::forget(&files, replaced);
        Ok(())
    }

    fn link(&self, from: &str, to: &str) -> io::Result<()> {
        self.shared.dir.link(from, to)?;
        let mut files = self.shared.files.lock().unwrap();
        if let Some(linked) = files.get(from).cloned() {
            files.insert(to.to_owned(), linked);
        }
        Ok(())
    }

    fn create_dir(&self, path: &str) -> io::Result<()> {
        self.shared.dir.create_dir(path)
    }

    fn list(&self, path: &str) -> io::Result<Vec<String>> {
        self.shared.dir.list(path)
    }

    fn sync_dir(&self, path: &str) -> io::Result<()> {
        self.shared.dir.sync_dir(path)
    }

    fn barrier(&self) -> io::Result<()> {
        self.shared.dir.barrier()
    }

    fn take(&self) -> io::Result<()> {
        self.shared.dir.take()
    }

    fn let_go(&self) {
        self.shared.dir.let_go()
    }

    fn claim(&self) -> io::Result<()> {
        self.shared.dir.claim()
    }

    fn unclaim(&self) {
        self.shared.dir.unclaim()
    }

    /// A file handed to the host by its path, as the host's cache of objects takes one by renaming
    /// it (TASK-165.8), has its dirty pages written back first, and its pages dropped: the host
    /// reads and moves it around the cache.
    fn host_path(&self, path: &str) -> Option<PathBuf> {
        if let Some(cached) = self.cached(path) {
            if let Err(err) = cached.settle(0..u64::MAX) {
                log::warn!("extent store: writing {path} back for the host: {err}");
                return None;
            }
            self.shared.files.lock().unwrap().remove(path);
            cached.cancel();
        }
        self.shared.dir.host_path(path)
    }

    fn concurrent(&self) -> bool {
        self.shared.dir.concurrent()
    }

    fn disk(&self) -> Option<&Arc<bucket_cell::host_disk::HostDisk>> {
        self.shared.dir.disk()
    }
}

/// A file of a [`CachedDir`]: one of its extents' files, through the page cache, or any other, as
/// the directory under it has it.
pub enum CachedFile<F: File> {
    /// A file of no extents', as it is.
    Raw(F),
    /// An extents' file, through the page cache.
    Cached(Cached<F>),
}

impl<F: File> CachedFile<F> {
    /// The file under the cache.
    pub fn under(&self) -> &F {
        match self {
            Self::Raw(file) => file,
            Self::Cached(cached) => cached.file(),
        }
    }

    /// Writes back the pages the cache holds dirty in `range`, if the file is cached: what an I/O
    /// of a caller's own on the file under it ([`AsRawFd`]) is to be preceded by, as an image's
    /// device's is, so that it reads what was written.
    pub fn settle(&self, range: Range<u64>) -> io::Result<()> {
        match self {
            Self::Raw(_) => Ok(()),
            Self::Cached(cached) => cached.settle(range),
        }
    }

    /// As [`CachedFile::settle`] does, before a write of a caller's own on the file under it,
    /// which also drops the pages there and takes the file to go as far as `range` at least, so
    /// that what the caller writes no page hides, and reads see all of it.
    pub fn settle_for_write(&self, range: Range<u64>) -> io::Result<()> {
        match self {
            Self::Raw(_) => Ok(()),
            Self::Cached(cached) => cached.settle_for_write(range),
        }
    }
}

impl<F: File + AsRawFd> AsRawFd for CachedFile<F> {
    fn as_raw_fd(&self) -> RawFd {
        self.under().as_raw_fd()
    }
}

impl<F: File> File for CachedFile<F> {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        match self {
            Self::Raw(file) => file.read_at(buf, offset),
            Self::Cached(cached) => cached.read_at(buf, offset),
        }
    }

    fn read_cached(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        match self {
            Self::Raw(file) => file.read_cached(buf, offset),
            Self::Cached(cached) => cached.read_cached(buf, offset),
        }
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        match self {
            Self::Raw(file) => file.write_at(buf, offset),
            Self::Cached(cached) => cached.write_at(buf, offset),
        }
    }

    fn write_back(&self, offset: u64, len: u64) -> io::Result<()> {
        match self {
            Self::Raw(file) => file.write_back(offset, len),
            Self::Cached(cached) => cached.write_back(offset, len),
        }
    }

    fn write_behind(&self, offset: u64, len: u64) -> io::Result<()> {
        match self {
            Self::Raw(file) => file.write_behind(offset, len),
            Self::Cached(cached) => cached.write_behind(offset, len),
        }
    }

    fn size(&self) -> io::Result<u64> {
        match self {
            Self::Raw(file) => file.size(),
            Self::Cached(cached) => cached.size(),
        }
    }

    fn sync(&self) -> io::Result<()> {
        match self {
            Self::Raw(file) => file.sync(),
            Self::Cached(cached) => cached.sync(),
        }
    }

    fn sync_cached(&self) -> io::Result<()> {
        match self {
            Self::Raw(file) => file.sync_cached(),
            Self::Cached(cached) => cached.sync_cached(),
        }
    }
}
