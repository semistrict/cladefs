// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a cell directory whose every change is counted in its footprint as it is made
//! ([`crate::footprint`], backlog doc-22, TASK-539.4): what every directory a cell opens is
//! outermost, over its page cache ([`crate::CachedDir`]), the host's ([`crate::HostDir`]) and one
//! in memory ([`crate::MemDir`]) alike, so that no cell, a test's no more than a host's, runs
//! around it. A write is counted as the page cache takes it; what changes a directory, as it is
//! changed, once the directory under it has changed it.

use crate::cached::CachedDir;
use crate::dir::{Dir, DirFile, File};
use crate::footprint::{FileLen, Footprint};
use crate::pages::PageCache;
use bucket_cell::host_disk::HostDisk;
use std::io;
use std::path::Path;
use std::sync::Arc;

/// A directory, `D`, its changes counted. See the module docs.
pub struct Metered<D: Dir> {
    shared: Arc<Shared<D>>,
}

struct Shared<D: Dir> {
    dir: D,
    footprint: Arc<Footprint>,
}

/// Its clones are the same opening of its cell.
impl<D: Dir> Clone for Metered<D> {
    fn clone(&self) -> Self {
        Self { shared: self.shared.clone() }
    }
}

impl<D: Dir> Drop for Shared<D> {
    fn drop(&mut self) {
        self.footprint.detach();
    }
}

impl<D: Dir> Metered<D> {
    /// `dir`, its changes counted in `footprint`, its cell's: listed, if no other opening of it
    /// is alive, and counted as listed ([`Footprint::attach`]).
    pub fn over(dir: D, footprint: Arc<Footprint>) -> io::Result<Self> {
        footprint.attach(&dir)?;
        Ok(Self { shared: Arc::new(Shared { dir, footprint }) })
    }

    /// The directory under it.
    pub fn under(&self) -> &D {
        &self.shared.dir
    }

    /// Its cell's footprint.
    pub fn footprint(&self) -> &Arc<Footprint> {
        &self.shared.footprint
    }

    fn file(&self, file: D::File, len: Arc<FileLen>) -> MeteredFile<D::File> {
        MeteredFile { file, len, footprint: self.shared.footprint.clone() }
    }
}

impl<D: Dir> Metered<CachedDir<D>> {
    /// The page cache its files' pages are in.
    pub fn cache(&self) -> &Arc<PageCache> {
        self.under().cache()
    }
}

impl<D: Dir> Dir for Metered<D> {
    type File = MeteredFile<D::File>;

    fn create(&self, path: &str, data: bool) -> io::Result<Self::File> {
        let file = self.shared.dir.create(path, data)?;
        Ok(self.file(file, self.shared.footprint.made(path)))
    }

    fn open(&self, path: &str, data: bool) -> io::Result<Self::File> {
        let file = self.shared.dir.open(path, data)?;
        let len = self.shared.footprint.opened(path, || file.size())?;
        Ok(self.file(file, len))
    }

    fn remove(&self, path: &str) -> io::Result<()> {
        self.shared.dir.remove(path)?;
        self.shared.footprint.removed(path);
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.shared.dir.rename(from, to)?;
        self.shared.footprint.renamed(from, to);
        Ok(())
    }

    fn link(&self, from: &str, to: &str) -> io::Result<()> {
        self.shared.dir.link(from, to)?;
        self.shared.footprint.linked(from, to);
        Ok(())
    }

    fn replace(&self, path: &str, bytes: &[u8]) -> io::Result<()> {
        self.shared.dir.replace(path, bytes)?;
        self.shared.footprint.replaced(path, bytes.len() as u64);
        Ok(())
    }

    fn create_dir(&self, path: &str) -> io::Result<()> {
        self.shared.dir.create_dir(path)?;
        self.shared.footprint.made_dir(path);
        Ok(())
    }

    fn list(&self, path: &str) -> io::Result<Vec<String>> {
        self.shared.dir.list(path)
    }

    fn entries(&self) -> io::Result<Vec<DirFile>> {
        self.shared.dir.entries()
    }

    crate::dir::delegate_dir_passthrough!();

    /// The first taking in this process lists it anew ([`Footprint::taken`]): other processes may
    /// have changed it while they had it.
    fn take(&self) -> io::Result<()> {
        self.shared.dir.take()?;
        self.shared.footprint.taken(&self.shared.dir).inspect_err(|_| self.shared.dir.let_go())
    }

    fn let_go(&self) {
        self.shared.footprint.let_go();
        self.shared.dir.let_go()
    }

    /// Taken, it goes from the cell, as a removal takes it.
    fn give(&self, path: &str, to: &mut dyn FnMut(&Path) -> io::Result<bool>) -> io::Result<bool> {
        let given = self.shared.dir.give(path, to)?;
        if given {
            self.shared.footprint.removed(path);
        }
        Ok(given)
    }

    fn concurrent(&self) -> bool {
        self.shared.dir.concurrent()
    }

    fn disk(&self) -> Option<&Arc<HostDisk>> {
        self.shared.dir.disk()
    }

    fn footprint(&self) -> Option<&Arc<Footprint>> {
        Some(&self.shared.footprint)
    }

    fn host(&self) -> Option<&Arc<crate::Host>> {
        self.shared.dir.host()
    }
}

/// A file of a [`Metered`] directory: its writes counted as far as they go.
pub struct MeteredFile<F: File> {
    file: F,
    len: Arc<FileLen>,
    footprint: Arc<Footprint>,
}

impl<F: File> MeteredFile<F> {
    /// The file under it.
    pub fn under(&self) -> &F {
        &self.file
    }
}

impl<F: File> File for MeteredFile<F> {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.file.read_at(buf, offset)
    }

    /// Counted as far as it went: one failing may have grown the file part way, as a partial
    /// write does, which its size says.
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        let written = self.file.write_at(buf, offset);
        match &written {
            Ok(()) => self.len.grow(&self.footprint, offset + buf.len() as u64),
            // Failed: its length what the file holds now; the error is the caller's.
            Err(_) => {
                if let Ok(size) = self.file.size() {
                    self.len.grow(&self.footprint, size);
                }
            }
        }
        written
    }

    fn write_back(&self, offset: u64, len: u64) -> io::Result<()> {
        self.file.write_back(offset, len)
    }

    fn write_behind(&self, offset: u64, len: u64) -> io::Result<()> {
        self.file.write_behind(offset, len)
    }

    fn size(&self) -> io::Result<u64> {
        self.file.size()
    }

    fn sync(&self) -> io::Result<()> {
        self.file.sync()
    }

    /// cladefs: counted as the blocks it takes after (TASK-698.2).
    fn punch(&self, offset: u64, len: u64) -> io::Result<()> {
        self.file.punch(offset, len)?;
        self.len.settle(&self.footprint, self.file.allocated()?);
        Ok(())
    }

    fn allocated(&self) -> io::Result<u64> {
        self.file.allocated()
    }
}
