// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A VM's memory as a vdisk of its cell (TASK-456.2): a store the host's extent daemon makes as
//! a memory vdisk (`OPEN_MEMORY`), kept as an `extent_files` file set of one file, [`IMAGE`], the
//! image of the guest's memory, a page a block. A snapshot writes the pages its guest wrote and
//! makes the zero ones holes, which store nothing, and commits them at once ([`MemoryVdisk`]); a
//! restored VM's next snapshot goes into a fork of its parent's memory ([`MemoryVdisk::fork`]),
//! which reads the parent's pages and stores only its own.
//!
//! Its device is the daemon's store ([`DaemonDevice`]): its reads of sealed extents are the
//! daemon's cache's, which every store and VM it serves shares.

use crate::error::Error;
use crate::{DaemonConnection, DaemonDevice, DaemonExtentStore};
use extent_files::{FileSet, Options};
use std::path::Path;
use std::sync::Arc;
use storage_device::DeviceHolder;

/// The file of a memory vdisk that is the guest's memory.
pub const IMAGE: &str = "memory";

/// The store capacity a memory of `size` bytes takes at most: its pages, the tree naming them (a
/// node of 170 refs a page) and their owners (8 bytes a page), and room for a commit's worth of
/// pages written again besides its compactions.
pub fn capacity(size: u64) -> u64 {
    size + size / 128 + size / 512 + (64 << 20)
}

/// A VM's memory, a vdisk of its cell. See the module docs.
pub struct MemoryVdisk {
    store: Arc<DaemonExtentStore>,
    files: Arc<FileSet>,
    size: u64,
}

impl MemoryVdisk {
    /// The memory vdisk at `store`, which the daemon listening at `socket` serves, made, a
    /// memory of `size` bytes of zeros, if it doesn't exist: `EINVAL` for one of another size.
    pub fn open(socket: &Path, store: &Path, size: u64) -> Result<Self, Error> {
        let daemon = DaemonConnection::connect(socket)?;
        let opened = daemon.open_memory(store, capacity(size))?;
        let store = Arc::new(opened);
        let device = DeviceHolder::new(DaemonDevice::new(store.clone()).map_err(Error::new)?);
        let files = FileSet::open(device, Options::default()).map_err(Error::new)?;
        files.open_file(IMAGE, true).map_err(Error::new)?;
        match files.size(IMAGE).map_err(Error::new)? {
            0 if size > 0 => {
                files.truncate(IMAGE, size).map_err(Error::new)?;
                files.sync(IMAGE).map_err(Error::new)?;
            }
            held if held == size => {}
            held => {
                return Err(Error::new(crate::Errno::EINVAL)
                    .context(format!("a memory of {held} bytes, not {size}")));
            }
        }
        Ok(Self { store, files, size })
    }

    /// The bytes of the guest's memory.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The bytes of a page, which reads, writes and holes are whole ones of.
    pub fn page_size(&self) -> u64 {
        self.files.device().block_size() as u64
    }

    /// The branch of the vdisk's store.
    pub fn branch(&self) -> u64 {
        self.store.branch()
    }

    /// Writes `pages` at `offset`, whole pages, to be committed by [`MemoryVdisk::commit`].
    pub fn write(&self, offset: u64, pages: &[u8]) -> Result<(), Error> {
        self.whole(offset, pages.len() as u64)?;
        self.files.write(IMAGE, offset, pages).map_err(Error::new)
    }

    /// Makes the `len` bytes at `offset`, whole pages, holes, which read as zeros and store
    /// nothing, once committed.
    pub fn punch(&self, offset: u64, len: u64) -> Result<(), Error> {
        self.whole(offset, len)?;
        self.files.punch(IMAGE, offset, len).map_err(Error::new)
    }

    /// Commits what was written and punched since the last commit, at once, durably on the
    /// host's disk: a crash leaves the memory as one commit or the other.
    pub fn commit(&self) -> Result<(), Error> {
        self.files.sync(IMAGE).map_err(Error::new)
    }

    /// Reads into `buf` from `offset`, as written.
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), Error> {
        let read = self.files.read(IMAGE, offset, buf).map_err(Error::new)?;
        match read == buf.len() {
            true => Ok(()),
            false => Err(Error::new(crate::Errno::EINVAL).context(format!(
                "reading {} bytes at {offset} of {}",
                buf.len(),
                self.size
            ))),
        }
    }

    /// Forks the vdisk, as of its last commit, to a new store at `path`, a memory vdisk of a
    /// new cell, which reads its pages until it writes its own: its branch.
    pub fn fork(&self, path: &Path) -> Result<u64, Error> {
        self.store.fork(path).map_err(Error::new)
    }

    /// The extents the vdisk holds, read whole: those its image's last commit names, and those
    /// it has yet to free.
    pub fn extents(&self) -> Result<Vec<storage_device::ExtentId>, Error> {
        self.files.extents().map_err(Error::new)
    }

    /// Closes the vdisk, every commit durable already.
    pub fn close(self) -> Result<(), Error> {
        self.files.close_file(IMAGE);
        self.files.close().map_err(Error::new)
    }

    /// Fails with `EINVAL` unless `len` bytes at `offset` are whole pages within the memory.
    fn whole(&self, offset: u64, len: u64) -> Result<(), Error> {
        let page = self.page_size();
        let within = offset.checked_add(len).is_some_and(|end| end <= self.size);
        match offset.is_multiple_of(page) && len.is_multiple_of(page) && within {
            true => Ok(()),
            false => Err(Error::new(crate::Errno::EINVAL).context(format!(
                "{len} bytes at {offset}: not whole pages of {page} within {}",
                self.size
            ))),
        }
    }
}
