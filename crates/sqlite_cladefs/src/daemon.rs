// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A database on a cell the host's extent daemon serves (TASK-300): its reads, and its window's
//! pages, are the daemon's cache's, the one copy of each extent every cell and VM it serves
//! shares; its writes and commits are the daemon's requests, as a VM's are.
//!
//! The window is a DAX window in this process's memory, as a VM process's is (doc-13): the daemon
//! maps its cache's pages there, read-only, and this process's faults there go to it, through
//! userfaultfd. So a page SQLite reads in place is the cache's page itself (or, on a host whose
//! pages are larger than 4 KiB such as 16 KiB arm64, a direct or composed host page in the
//! daemon's budgeted slabs).
//!
//! Or, embedded, a database on a cell of this process's own daemon ([`create_embedded`], board
//! 13909): the open cell itself, with no connection, window or thread of its own, as a process
//! holding many databases needs.

use crate::{Database, Options};
use anyhow::{Context as _, Error, ensure};
use block_device::{Contents, Run, Window};
use cladefs::{CellClient, DaemonDevice, MappingContents, WindowMapping};
use std::path::Path;
use std::ptr::NonNull;
use std::sync::Arc;
use storage_device::{Device, DeviceHolder};

/// The database on the cell at `cell_dir` that the daemon listening at `socket` serves, made with
/// `capacity` bytes if it doesn't exist, its file mapped into a window of `window` bytes of this
/// process's address space, which takes memory only as the daemon maps its pages there: where the
/// host's pages fit the cell's blocks, unmapped elsewhere (see the module docs).
pub fn open(
    socket: &Path,
    cell_dir: &Path,
    capacity: u64,
    window: u64,
    options: Options,
) -> Result<Arc<Database>, Error> {
    // A SQLite cell, if the daemon makes it (TASK-364): one the FUSE mounts of its
    // cell leave be, as no filesystem's.
    let opened = cladefs::DaemonConnection::connect(socket)
        .and_then(|daemon| daemon.open_sqlite(cell_dir, capacity));
    let cell = opened.map_err(Error::new).with_context(|| {
        format!("opening {} on the daemon at {}", cell_dir.display(), socket.display())
    })?;
    over(Arc::new(cell), window, options)
}

/// The database on `cell`, a SQLite cell already open on the daemon (as a caller that made it by
/// ID, `libcladefs::Daemon::create_embedded`, has it), mapped as [`open`] maps it.
pub fn over(cell: Arc<CellClient>, window: u64, options: Options) -> Result<Arc<Database>, Error> {
    let block = u64::from(cladefs::Storage::BLOCK_SIZE);
    if let Err(error) = cladefs::pages::window_fits_host(block, cladefs::pages::page_size()) {
        log::info!("sqlite_cladefs: a database with no window, reading from the daemon: {error:#}");
        return Database::open(DeviceHolder::new(DaemonDevice::new(cell)?), options);
    }
    // The window first, holding the cell alone: one whose making fails lets the cell go, then
    // its memory (`Drop`). The device after it, which a block device drops before its window,
    // opened or not (block_device's `Window`): so that a failed opening unmaps the window too,
    // which the device's hold on the cell kept mapped for the process's life (board 8551).
    let window = DaemonWindow::new(cell, window)?;
    let device = DeviceHolder::new(DaemonDevice::new(window.cell.clone().expect("a cell"))?);
    assert_eq!(device.block_size() as u64, block, "a daemon cell's blocks");
    Database::open_mapped(device, Box::new(window), options)
}

/// cladefs: a database on a new SQLite cell of `tenant`'s, of `capacity` bytes, on a disk of this
/// process's own daemon, open in its process with no VM-process wire (`cladefsd::embedded`, board
/// 13909): no socket, window or thread of its own, its reads the open cell's. The cell's ID, and
/// the database, which holds the cell open until it is closed and dropped.
pub fn create_embedded(
    daemon: &Arc<cladefs::cladefsd::Daemon>,
    tenant: u64,
    capacity: u64,
    options: Options,
) -> Result<(cladefs::CellId, Arc<Database>), Error> {
    use cladefs::cladefsd::embedded::Kind;
    let made = futures::executor::block_on(daemon.create_embedded(tenant, capacity, Kind::Sqlite));
    let cell = made.map_err(Error::new).context("making an embedded SQLite cell")?;
    let id = cell.id();
    Ok((id, over_embedded(cell, options)?))
}

/// cladefs: the database on the existing SQLite cell `cell`, kept on a disk of this process's own
/// daemon, open in its process as [`create_embedded`]'s is.
pub fn open_embedded(
    daemon: &Arc<cladefs::cladefsd::Daemon>,
    cell: cladefs::CellId,
    options: Options,
) -> Result<Arc<Database>, Error> {
    let opened = futures::executor::block_on(daemon.open_embedded(cell));
    let opened = opened.map_err(Error::new).with_context(|| format!("opening cell {cell}"))?;
    over_embedded(opened, options)
}

/// The database on `cell`'s device, which holds the cell from then on.
fn over_embedded(
    cell: Arc<cladefs::cladefsd::embedded::EmbeddedCell>,
    options: Options,
) -> Result<Arc<Database>, Error> {
    let device = cell.device()?;
    drop(cell);
    Database::open(DeviceHolder::new(device), options)
}

/// A cell's DAX window in this process's memory. See the module docs.
pub struct DaemonWindow {
    /// The cell, which goes before the memory does.
    cell: Option<Arc<CellClient>>,
    memory: NonNull<u8>,
    size: u64,
}

// SAFETY: the memory is the daemon's to change, through the cell, whose requests may come from
// any thread; this process only reads it.
unsafe impl Send for DaemonWindow {}
// SAFETY: as Send's: shared, this process only reads the memory, which the daemon changes.
unsafe impl Sync for DaemonWindow {}

impl DaemonWindow {
    /// `cell`'s window, in `size` bytes of new address space.
    pub fn new(cell: Arc<CellClient>, size: u64) -> Result<Self, Error> {
        // SAFETY: new anonymous memory, which the window takes over.
        let memory = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        ensure!(
            memory != libc::MAP_FAILED,
            "mapping {size} bytes for the window: {}",
            std::io::Error::last_os_error()
        );
        let memory = NonNull::new(memory.cast::<u8>()).expect("a mapping");
        let window = Self { cell: Some(cell), memory, size };
        // A writer's window copies on write, of one private page at least; this process never
        // writes it, so that none is ever made.
        // SAFETY: the memory is this window's alone, and outlives the cell (`Drop`).
        unsafe { window.cell().create_dax_window_in_host_memory(memory, size, None, 1) }
            .map_err(Error::new)
            .context("creating the window")?;
        Ok(window)
    }

    fn cell(&self) -> &CellClient {
        self.cell.as_ref().expect("a cell until dropped")
    }
}

impl Window for DaemonWindow {
    fn memory(&self) -> (NonNull<u8>, u64) {
        (self.memory, self.size)
    }

    fn map(&self, _device: &dyn Device, runs: &[Run]) -> Result<(), Error> {
        let mappings: Vec<_> = (runs.iter())
            .map(|run| WindowMapping {
                window_offset: run.at,
                len: run.len,
                contents: match run.contents {
                    Contents::Extent { extent, offset } => {
                        MappingContents::Extent { extent, offset }
                    }
                    Contents::Zeros => MappingContents::Zeros,
                },
            })
            .collect();
        self.cell().map_many(&mappings).map_err(Error::new).context("mapping the window")
    }

    fn unmap(&self, at: u64, len: u64) -> Result<(), Error> {
        self.cell().unmap(at, len).map_err(Error::new).context("unmapping the window")
    }
}

impl Drop for DaemonWindow {
    fn drop(&mut self) {
        // The memory goes once the cell, whose fault thread serves it, has: unless something
        // holds the cell still, when the memory stays, as the cell may yet map it. Nothing does
        // where its holders go before it, as `open`'s do, and a block device's.
        match Arc::try_unwrap(self.cell.take().expect("a cell")) {
            Ok(cell) => drop(cell),
            // Held still: a bug of its holders'; its memory stays mapped for the cell.
            Err(_) => {
                log::error!("sqlite_cladefs: a window's cell outlives it: its memory stays mapped");
                return;
            }
        }
        // SAFETY: the window's memory, which nothing uses with the cell gone.
        unsafe { libc::munmap(self.memory.as_ptr().cast(), self.size as usize) };
    }
}
