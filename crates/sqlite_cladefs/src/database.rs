// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A SQLite cell's database: its file is a block device (TASK-456.1, board 5790), with what SQLite
//! needs besides of a VFS: its lock levels on the file, and its journals and temporary files.
//!
//! **One database.** A cell holds one database, its block device's contents. SQLite opens it by
//! a name, and the first name it opens it by is the database's for as long as the
//! [`Database`] lives: another, a second database's (`ATTACH`, or a connection opening another
//! file), is refused ([`OneDatabase`]), as it would open the same blocks as another database.
//! The name is not kept on the cell: opened again, the database is opened by any name.
//!
//! **Memory files.** Journals and temporary files live in memory: the database's file changes
//! only at its commits, whole, so that no crash needs a journal to roll it back.
//!
//! **Commits.** A transaction is committed as SQLite syncs the database's file, or, if told not
//! to (`synchronous=OFF`), as a writer lowers its lock: the block device's flush.

use crate::locks::{Level, Locks};
use anyhow::{Error, bail};
use block_device::{BlockDevice, Options, Window};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use storage_device::{DeviceHolder, ExtentId};

/// A file in memory, a journal or a temporary file.
pub type MemoryFile = Arc<Mutex<Vec<u8>>>;

/// The error of opening a journal that isn't there.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the database holds no file {0}")]
pub struct NotFound(pub String);

/// The error of opening a second database on a cell, which holds one.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a SQLite cell holds one database, opened as {held}: {refused} would be a second")]
pub struct OneDatabase {
    pub held: String,
    pub refused: String,
}

/// What SQLite holds of the database besides its file, under one lock.
#[derive(Default)]
struct Held {
    /// The name SQLite first opened the database by, which alone opens it.
    name: Option<String>,
    /// How many connections have it open.
    opens: usize,
    /// The connections' locks of the database's file.
    locks: Locks,
    /// The journals, by name. Temporary files have none.
    memory: HashMap<String, MemoryFile>,
}

/// A SQLite cell's database. See the module docs.
pub struct Database {
    blocks: Arc<BlockDevice>,
    held: Mutex<Held>,
}

impl Database {
    fn new(blocks: Arc<BlockDevice>) -> Arc<Self> {
        Arc::new(Self { blocks, held: Mutex::default() })
    }

    /// The database on `device`, made empty if the device's root is.
    pub fn open(device: DeviceHolder, options: Options) -> Result<Arc<Self>, Error> {
        Ok(Self::new(BlockDevice::open(device, options)?))
    }

    /// The database on `device`, as [`Database::open`] opens it, its file mapped into `window`.
    pub fn open_mapped(
        device: DeviceHolder,
        window: Box<dyn Window>,
        options: Options,
    ) -> Result<Arc<Self>, Error> {
        Ok(Self::new(BlockDevice::open_mapped(device, window, options)?))
    }

    /// Closes the device: every commit is durable already.
    pub fn close(&self) -> Result<(), Error> {
        self.blocks.close()
    }

    /// The block device the database's file is.
    pub fn blocks(&self) -> &BlockDevice {
        &self.blocks
    }

    /// The extent device.
    pub fn device(&self) -> &DeviceHolder {
        self.blocks.device()
    }

    /// The extents it holds, in order.
    pub fn extents(&self) -> Result<Vec<ExtentId>, Error> {
        self.blocks.extents()
    }

    /// How many connections have the database open.
    pub fn opens(&self) -> usize {
        self.held.lock().unwrap().opens
    }

    /// Opens the database as `name`: the name it was first opened by, if any, else its name from
    /// now on. [`OneDatabase`] for another.
    pub fn open_database(&self, name: &str) -> Result<(), Error> {
        self.blocks.check()?;
        let mut held = self.held.lock().unwrap();
        match &held.name {
            Some(database) if database != name => {
                bail!(OneDatabase { held: database.clone(), refused: name.to_owned() })
            }
            Some(_) => {}
            None => held.name = Some(name.to_owned()),
        }
        held.opens += 1;
        Ok(())
    }

    /// Closes the database, opened once more than closed.
    pub fn close_database(&self) {
        let mut held = self.held.lock().unwrap();
        held.opens = held.opens.checked_sub(1).expect("the database open");
    }

    /// Where the `len` bytes of the database's file at `offset` are in the window, if it maps them
    /// as they read: for SQLite to read in place until it gives them back ([`Database::unfetch`]).
    pub fn fetch(&self, offset: u64, len: u64) -> Result<Option<*const u8>, Error> {
        self.blocks.fetch(offset, len)
    }

    /// Whether the database's file is mapped into a window, which SQLite's reads in place need
    /// ([`Database::fetch`]).
    pub fn is_mapped(&self) -> bool {
        self.blocks.is_mapped()
    }

    /// How many times SQLite fetched pages to read in place ([`Database::fetch`]).
    pub fn fetched(&self) -> u64 {
        self.blocks.fetched()
    }

    /// Gives back a page fetched ([`Database::fetch`]).
    pub fn unfetch(&self) {
        self.blocks.unfetch()
    }

    pub fn read(&self, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        self.blocks.read(offset, buf)
    }

    pub fn write(&self, offset: u64, data: &[u8]) -> Result<(), Error> {
        self.blocks.write(offset, data)
    }

    pub fn truncate(&self, size: u64) -> Result<(), Error> {
        self.blocks.resize(size)
    }

    pub fn size(&self) -> Result<u64, Error> {
        self.blocks.size()
    }

    /// Commits what changed in the database's file since its last commit, durably.
    pub fn sync(&self) -> Result<(), Error> {
        self.blocks.flush()
    }

    /// Raises a connection's lock of the database, `held`, to `want`: whether it got there.
    pub fn lock(&self, held: &mut Level, want: Level) -> Result<bool, Error> {
        self.blocks.check()?;
        Ok(self.held.lock().unwrap().locks.lock(held, want))
    }

    /// Lowers a connection's lock of the database, `held`, to `want`, committing what it changed
    /// first if it was writing, unless the block device failed: SQLite syncs at a transaction's
    /// end unless told not to (`synchronous=OFF`), and the transaction is committed then.
    pub fn unlock(&self, held: &mut Level, want: Level) -> Result<(), Error> {
        let mut locks = self.held.lock().unwrap();
        let committed = match *held > Level::Shared && self.blocks.check().is_ok() {
            true => self.blocks.flush(),
            false => Ok(()),
        };
        locks.locks.unlock(held, want);
        committed
    }

    /// Whether a connection holds a lock of the database past shared.
    pub fn reserved(&self) -> Result<bool, Error> {
        self.blocks.check()?;
        Ok(self.held.lock().unwrap().locks.reserved())
    }

    /// Whether file `name`, the database's or in memory, exists.
    pub fn exists(&self, name: &str) -> Result<bool, Error> {
        self.blocks.check()?;
        let held = self.held.lock().unwrap();
        Ok(held.memory.contains_key(name) || held.name.as_deref() == Some(name))
    }

    /// Deletes file `name`, in memory: whether it was there. The database's file is the cell's
    /// contents, which no delete takes.
    pub fn delete(&self, name: &str) -> Result<bool, Error> {
        self.blocks.check()?;
        let mut held = self.held.lock().unwrap();
        if held.name.as_deref() == Some(name) {
            bail!("deleting {name}, the database's file: a cell's contents are not deleted");
        }
        Ok(held.memory.remove(name).is_some())
    }

    /// The journal `name`, made empty if it isn't there and `create`.
    pub fn memory_file(&self, name: &str, create: bool) -> Result<MemoryFile, Error> {
        self.blocks.check()?;
        let mut held = self.held.lock().unwrap();
        match held.memory.get(name) {
            Some(file) => Ok(file.clone()),
            None if create => Ok(held.memory.entry(name.to_owned()).or_default().clone()),
            None => bail!(NotFound(name.to_owned())),
        }
    }

    /// Fails unless the block device is usable: what its users see as an error of their own.
    pub fn check(&self) -> Result<(), Error> {
        self.blocks.check()
    }
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database").field("blocks", &self.blocks).finish_non_exhaustive()
    }
}
