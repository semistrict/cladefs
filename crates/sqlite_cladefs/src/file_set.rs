// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A file set as SQLite uses it: an extent_files file set (TASK-456.1), whose files are the
//! databases' files, with what SQLite needs besides of a VFS: its lock levels on each file, and its
//! journals and temporary files.
//!
//! **Memory files.** Journals and temporary files live in memory: a database's file changes only
//! at its commits, whole, so that no crash needs a journal to roll it back.
//!
//! **Commits.** A transaction is committed as SQLite syncs the database's file, or, if told not
//! to (`synchronous=OFF`), as a writer lowers its lock: the file set commits the file alone
//! (`extent_files`).

use crate::locks::{Level, Locks};
use anyhow::{Error, bail};
use extent_files::{FileSetWindow, NotFound, Options};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use storage_device::{DeviceHolder, ExtentId};

/// A file in memory, a journal or a temporary file.
pub type MemoryFile = Arc<Mutex<Vec<u8>>>;

/// What SQLite holds of a file set besides its files, under one lock.
#[derive(Default)]
struct Held {
    /// The connections' locks of each database's file.
    locks: HashMap<String, Locks>,
    /// The journals, by name. Temporary files have none.
    memory: HashMap<String, MemoryFile>,
}

/// A file set of SQLite's databases. See the module docs.
pub struct FileSet {
    files: Arc<extent_files::FileSet>,
    held: Mutex<Held>,
}

impl FileSet {
    fn new(files: Arc<extent_files::FileSet>) -> Arc<Self> {
        Arc::new(Self { files, held: Mutex::default() })
    }

    /// The file set on `device`, made if the device's root is empty.
    pub fn open(device: DeviceHolder, options: Options) -> Result<Arc<Self>, Error> {
        Ok(Self::new(extent_files::FileSet::open(device, options)?))
    }

    /// The file set on `device`, as [`FileSet::open`] opens it, its files mapped into `window`, a
    /// region each, as many regions as `options` says.
    pub fn open_mapped(
        device: DeviceHolder,
        window: Box<dyn FileSetWindow>,
        options: Options,
    ) -> Result<Arc<Self>, Error> {
        Ok(Self::new(extent_files::FileSet::open_mapped(device, window, options)?))
    }

    /// Closes the device: every commit is durable already.
    pub fn close(&self) -> Result<(), Error> {
        self.files.close()
    }

    /// The device.
    pub fn device(&self) -> &DeviceHolder {
        self.files.device()
    }

    /// The databases' files it holds, committed or open.
    pub fn files(&self) -> Result<Vec<String>, Error> {
        self.files.files()
    }

    /// The extents it holds, in order.
    pub fn extents(&self) -> Result<Vec<ExtentId>, Error> {
        self.files.extents()
    }

    /// How many connections have its files open.
    pub fn opens(&self) -> usize {
        self.files.opens()
    }

    /// Opens database file `name`, made empty if it isn't there and `create`.
    pub fn open_file(&self, name: &str, create: bool) -> Result<(), Error> {
        self.files.open_file(name, create)
    }

    /// Closes database file `name`, opened once more than closed.
    pub fn close_file(&self, name: &str) {
        self.files.close_file(name)
    }

    /// Where the `len` bytes of file `name` at `offset` are in the window, if it maps them as
    /// they read: for SQLite to read in place until it gives them back ([`FileSet::unfetch`]).
    pub fn fetch(&self, name: &str, offset: u64, len: u64) -> Result<Option<*const u8>, Error> {
        self.files.fetch(name, offset, len)
    }

    /// Whether its files are mapped into a window, which SQLite's reads in place need
    /// ([`FileSet::fetch`]).
    pub fn is_mapped(&self) -> bool {
        self.files.is_mapped()
    }

    /// How many times SQLite fetched pages to read in place ([`FileSet::fetch`]).
    pub fn fetched(&self) -> u64 {
        self.files.fetched()
    }

    /// Gives back a page of file `name` fetched ([`FileSet::fetch`]).
    pub fn unfetch(&self, name: &str) {
        self.files.unfetch(name)
    }

    pub fn read(&self, name: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        self.files.read(name, offset, buf)
    }

    pub fn write(&self, name: &str, offset: u64, data: &[u8]) -> Result<(), Error> {
        self.files.write(name, offset, data)
    }

    pub fn truncate(&self, name: &str, size: u64) -> Result<(), Error> {
        self.files.truncate(name, size)
    }

    pub fn size(&self, name: &str) -> Result<u64, Error> {
        self.files.size(name)
    }

    /// Commits what changed in file `name` since its last commit, durably.
    pub fn sync(&self, name: &str) -> Result<(), Error> {
        self.files.sync(name)
    }

    /// Raises a connection's lock of file `name`, `held`, to `want`: whether it got there.
    pub fn lock_file(&self, name: &str, held: &mut Level, want: Level) -> Result<bool, Error> {
        self.files.check()?;
        let mut locks = self.held.lock().unwrap();
        Ok(locks.locks.entry(name.to_owned()).or_default().lock(held, want))
    }

    /// Lowers a connection's lock of file `name`, `held`, to `want`, committing what it changed
    /// first if it was writing, unless the file set failed: SQLite syncs at a transaction's end
    /// unless told not to (`synchronous=OFF`), and the transaction is committed then.
    pub fn unlock_file(&self, name: &str, held: &mut Level, want: Level) -> Result<(), Error> {
        let mut locks = self.held.lock().unwrap();
        let committed = match *held > Level::Shared && self.files.check().is_ok() {
            true => self.files.sync(name),
            false => Ok(()),
        };
        locks.locks.entry(name.to_owned()).or_default().unlock(held, want);
        committed
    }

    /// Whether a connection holds a lock of file `name` past shared.
    pub fn reserved(&self, name: &str) -> Result<bool, Error> {
        self.files.check()?;
        let locks = self.held.lock().unwrap();
        Ok(locks.locks.get(name).is_some_and(Locks::reserved))
    }

    /// Whether file `name`, a database's or in memory, exists.
    pub fn exists(&self, name: &str) -> Result<bool, Error> {
        self.files.check()?;
        let in_memory = self.held.lock().unwrap().memory.contains_key(name);
        Ok(in_memory || self.files.exists(name)?)
    }

    /// Deletes file `name`, a database's, durably, or in memory: whether it was there.
    pub fn delete(&self, name: &str) -> Result<bool, Error> {
        self.files.check()?;
        if self.held.lock().unwrap().memory.remove(name).is_some() {
            return Ok(true);
        }
        self.files.delete(name)
    }

    /// The journal `name`, made empty if it isn't there and `create`.
    pub fn memory_file(&self, name: &str, create: bool) -> Result<MemoryFile, Error> {
        self.files.check()?;
        let mut held = self.held.lock().unwrap();
        match held.memory.get(name) {
            Some(file) => Ok(file.clone()),
            None if create => Ok(held.memory.entry(name.to_owned()).or_default().clone()),
            None => bail!(NotFound(name.to_owned())),
        }
    }

    /// Fails unless the file set is usable: what its users see as an error of their own.
    pub fn check(&self) -> Result<(), Error> {
        self.files.check()
    }
}

impl std::fmt::Debug for FileSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileSet").field("files", &self.files).finish_non_exhaustive()
    }
}
