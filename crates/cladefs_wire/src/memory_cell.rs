// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A VM's memory as a cell of its own (TASK-456.2): one the host's extent daemon makes as
//! a memory cell (`OPEN_MEMORY`), kept as a block device (`block_device`) whose contents are the
//! image of the guest's memory, a page a block. A snapshot writes the pages its guest wrote and
//! discards the zero ones, holes, which store nothing, and commits them at once ([`MemoryCell`]); a
//! restored VM's next snapshot goes into a fork of its parent's memory ([`MemoryCell::make_version`]
//! and [`MemoryCell::open_forked`]), which reads the parent's pages and stores only its own.
//!
//! Its device is the daemon's extent store ([`DaemonDevice`]): its reads of sealed extents are the
//! daemon's cache's, which every cell and VM it serves shares.
//!
//! **Restored** (TASK-456.3), a VM's memory is the cell's window ([`MemoryCell::window`]): the
//! daemon's pages of its extents, shared with every VM restored from the same snapshot on the host,
//! mapped read-only, a page the guest writes to made a private copy of its own, as a DAX window
//! copies on write (backlog doc-13). The window maps nothing at first: a fault on a page of a tree
//! node not mapped yet maps that node's runs first ([`Unmapped`]), on a thread of the cell's own,
//! as reading the tree blocks, every fault on the node waiting for that one mapping; so a restore
//! reads the cell's root alone, and what of its tree the guest's touches reach. The node's pages
//! are installed with it, ahead of the guest's touches: KVM resolves a load of a page installed on
//! the vCPU, with no fault here, where a page not installed it hands to x86's asynchronous page
//! fault worker, whose fault is a write, a load's too, which the window can only take for a guest
//! write, a private copy (docs/research/kvm-async-pf-and-shared-guest-memory.md). So every write to
//! the window is a guest write, and a node's first page alone may be made private for a load. Its
//! private pages are the guest's memory, reserved nowhere in the daemon: the VM process's cgroup
//! limits them.
//!
//! What the guest gives back of its memory, a balloon's inflation or a free page report, is
//! reclaimed ([`MemoryCell::reclaim`]): the window maps the image's runs there again, as the guest's
//! `MAP` would, which ends its private pages there, their memory emptied, the host's again. The
//! guest reads the image's pages there next, as of the cell's last commit, which is what its
//! memory's next snapshot holds there too, the private pages being all a snapshot writes: the
//! balloon promises no contents for a page given back (Firecracker's offers no
//! `VIRTIO_BALLOON_F_PAGE_POISON`).
//!
//! A window maps the cell's runs at its 4 KiB block granularity; on hosts with larger pages (such
//! as 16 KiB arm64), the daemon composes unaligned or mixed runs into budgeted host pages and
//! shares aligned runs directly, while the mapper rounds its lazy-mapped node chunks up to whole
//! host pages so adjacent chunks never straddle a host page boundary.
//!
//! cladefs: on macOS (board 13994, board 14343), a window is a guest's RAM on Hypervisor.framework
//! ([`MemoryCell::window_in_ram`], `dax::hvf::ram`), which the VMM's devices read and write as
//! the guest does, with no userfaultfd to fault in what isn't there: its whole image is mapped as
//! the window is made, each data page the daemon's, mapped copy-on-write, each hole the RAM's
//! zeros, and a write of the guest's or the VMM's makes a page private, the kernel's copy. Its
//! private pages, the RAM's anonymous ones, are what a snapshot writes, as here; a reclaim maps
//! the image's pages anew as it does here, in whole host pages.

use crate::{CellClient, DaemonConnection, DaemonDevice};
use block_device::{BlockDevice, Contents, Options};
#[cfg(target_os = "macos")]
use cladefs_core::dax::hvf::{GuestRam, HvfRamStage};
use cladefs_core::error::{Errno, Error};
use cladefs_core::{CellVersion, MappingContents, WindowMapping};
use std::path::Path;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};
use storage_device::DeviceHolder;
#[cfg(target_os = "linux")]
use {
    crate::Unmapped,
    cladefs_core::Prefault,
    futures::channel::oneshot,
    futures::future::BoxFuture,
    std::collections::HashMap,
    std::sync::{Weak, mpsc},
};

/// A memory cell's window, once it has one: on Linux its tree nodes, which faults map as they
/// reach them ([`MemoryCell::window`]); on macOS the guest's RAM it is
/// ([`MemoryCell::window_in_ram`]).
#[cfg(target_os = "linux")]
type Window = Arc<Nodes>;
#[cfg(target_os = "macos")]
type Window = Arc<HvfRamStage>;

/// The cell capacity a memory of `size` bytes takes at most: its pages, the tree naming them (a
/// node of 170 refs a page) and their owners (8 bytes a page), and room for a commit's worth of
/// pages written again besides its compactions.
pub fn capacity(size: u64) -> u64 {
    cladefs_core::memory::memory_capacity(size)
}

/// A VM's memory, a cell of its own. See the module docs.
pub struct MemoryCell {
    client: Arc<CellClient>,
    blocks: Arc<BlockDevice>,
    size: u64,
    /// Its window, once it has one.
    window: Mutex<Option<Window>>,
}

impl MemoryCell {
    /// The memory cell at `cell_dir`, which the daemon listening at `socket` serves, made, a
    /// memory of `size` bytes of zeros, if it doesn't exist: `EINVAL` for one of another size.
    pub fn open(socket: &Path, cell_dir: &Path, size: u64) -> Result<Self, Error> {
        let daemon = DaemonConnection::connect(socket)?;
        Self::over(daemon.open_memory(cell_dir, capacity(size))?, size)
    }

    /// Opens an existing managed memory cell `cell` served by the daemon listening at `socket`
    /// (hotbed question 547).
    pub fn open_cell(socket: &Path, cell: cladefs_core::CellId, size: u64) -> Result<Self, Error> {
        let daemon = DaemonConnection::connect(socket)?;
        Self::over(daemon.open_memory(&cell.to_string(), capacity(size))?, size)
    }

    /// Creates and opens a new managed memory cell for `tenant` of `size` bytes on a disk chosen
    /// by the daemon listening at `socket` (hotbed question 547).
    pub fn create_cell(socket: &Path, tenant: u64, size: u64) -> Result<Self, Error> {
        let daemon = DaemonConnection::connect(socket)?;
        Self::over(daemon.create_memory_cell(tenant, capacity(size))?, size)
    }

    /// Forks a memory cell version `from` into a new managed memory cell for `tenant` of `size`
    /// bytes on a disk chosen by the daemon listening at `socket` (hotbed question 547).
    pub fn open_forked_cell(
        socket: &Path,
        tenant: u64,
        from: CellVersion,
        size: u64,
    ) -> Result<Self, Error> {
        let daemon = DaemonConnection::connect(socket)?;
        Self::over(daemon.open_forked_cell(tenant, from, capacity(size))?, size)
    }

    /// The memory cell at `cell_dir`, forked from the version `from` of a memory cell if it doesn't
    /// exist: a VM's memory restored from a snapshot, on any host, as its disks are
    /// (docs/integrator-guide.md). `EINVAL` for a memory of another size.
    pub fn open_forked(
        socket: &Path,
        cell_dir: &Path,
        from: CellVersion,
        size: u64,
    ) -> Result<Self, Error> {
        let daemon = DaemonConnection::connect(socket)?;
        Self::over(daemon.open_forked(cell_dir, from, capacity(size))?, size)
    }

    /// Creates (or reopens) a standalone host-local ephemeral memory cell at `cell_dir` of `size`
    /// bytes of zeros, writing nothing to the bucket. `EINVAL` if the cell already exists with
    /// another size, kind, or non-ephemeral mode.
    pub fn open_ephemeral(socket: &Path, cell_dir: &Path, size: u64) -> Result<Self, Error> {
        let daemon = DaemonConnection::connect(socket)?;
        Self::over(daemon.open_memory_ephemeral(cell_dir, capacity(size))?, size)
    }

    /// Reopens an existing host-local ephemeral memory cell at `cell_dir` in place (`ENOENT` if
    /// missing, `EINVAL` if not an ephemeral memory cell or wrong size, `EEXIST` if `expected` is
    /// provided and the cell ID on disk differs).
    pub fn open_ephemeral_existing(
        socket: &Path,
        cell_dir: &Path,
        expected: Option<cladefs_core::CellId>,
        size: u64,
    ) -> Result<Self, Error> {
        let daemon = DaemonConnection::connect(socket)?;
        Self::over(daemon.open_ephemeral_memory_existing(cell_dir, expected, capacity(size))?, size)
    }

    /// The memory cell `opened` is, of a memory of `size` bytes, made one of zeros if new.
    fn over(opened: CellClient, size: u64) -> Result<Self, Error> {
        let client = Arc::new(opened);
        let device = DeviceHolder::new(DaemonDevice::new(client.clone()).map_err(Error::new)?);
        let blocks = BlockDevice::open(device, Options::default()).map_err(Error::new)?;
        match blocks.size().map_err(Error::new)? {
            0 if size > 0 => {
                blocks.resize(size).map_err(Error::new)?;
                blocks.flush().map_err(Error::new)?;
            }
            held if held == size => {}
            held => {
                return Err(Error::new(cladefs_core::Errno::EINVAL)
                    .context(format!("a memory of {held} bytes, not {size}")));
            }
        }
        Ok(Self { client, blocks, size, window: Mutex::new(None) })
    }

    /// The bytes of the guest's memory.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The bytes of a page, which reads, writes and holes are whole ones of.
    pub fn page_size(&self) -> u64 {
        self.blocks.block_size()
    }

    /// Its cell.
    pub fn cell(&self) -> u64 {
        self.client.cell()
    }

    /// Its cell's tenant.
    pub fn tenant(&self) -> u64 {
        self.client.tenant()
    }

    /// Whether this memory cell is a host-local ephemeral cell (zero bucket uploads).
    pub fn is_ephemeral(&self) -> bool {
        self.client.is_ephemeral()
    }

    /// Writes `pages` at `offset`, whole pages, to be committed by [`MemoryCell::commit`].
    pub fn write(&self, offset: u64, pages: &[u8]) -> Result<(), Error> {
        self.whole(offset, pages.len() as u64)?;
        self.blocks.write(offset, pages).map_err(Error::new)
    }

    /// Discards the `len` bytes at `offset`, whole pages: holes, which read as zeros and store
    /// nothing, once committed.
    pub fn discard(&self, offset: u64, len: u64) -> Result<(), Error> {
        self.whole(offset, len)?;
        self.blocks.discard(offset, len).map_err(Error::new)
    }

    /// Commits what was written and discarded since the last commit, at once, durably on the
    /// host's disk: a crash leaves the memory as one commit or the other.
    pub fn commit(&self) -> Result<(), Error> {
        self.blocks.flush().map_err(Error::new)
    }

    /// Reads into `buf` from `offset`, as written.
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), Error> {
        let read = self.blocks.read(offset, buf).map_err(Error::new)?;
        match read == buf.len() {
            true => Ok(()),
            false => Err(Error::new(cladefs_core::Errno::EINVAL).context(format!(
                "reading {} bytes at {offset} of {}",
                buf.len(),
                self.size
            ))),
        }
    }

    /// Writes `bytes` at `offset`, whole pages: all-zero pages discarded as holes, which store
    /// nothing, and non-zero runs written, to be committed by [`MemoryCell::commit`].
    pub fn write_sparse(&self, offset: u64, bytes: &[u8]) -> Result<(), Error> {
        self.whole(offset, bytes.len() as u64)?;
        let page = self.page_size() as usize;
        for (at, run) in cladefs_core::memory::runs_of_zeros(bytes, page) {
            let at = offset + at as u64;
            match run {
                cladefs_core::memory::PageRun::Zeros(len) => self.discard(at, len as u64)?,
                cladefs_core::memory::PageRun::Data(data) => self.write(at, data)?,
            }
        }
        Ok(())
    }

    /// Writes the memory a file holds, a snapshot's memory file as Firecracker writes it, the
    /// memory's size: its pages, those of zeros as holes, which store nothing, committed at once.
    pub fn import(&self, file: &Path) -> Result<(), Error> {
        use std::io::Read as _;
        let mut file = std::fs::File::open(file).map_err(Error::new)?;
        let held = file.metadata().map_err(Error::new)?.len();
        if held != self.size {
            return Err(Error::new(Errno::EINVAL)
                .context(format!("a memory file of {held} bytes, not {}", self.size)));
        }
        let mut buffer = vec![0; cladefs_core::memory::MEMORY_IMPORT_BYTES];
        let mut offset = 0;
        while offset < self.size {
            let len =
                (self.size - offset).min(cladefs_core::memory::MEMORY_IMPORT_BYTES as u64) as usize;
            let chunk = &mut buffer[..len];
            file.read_exact(chunk).map_err(Error::new)?;
            self.write_sparse(offset, chunk)?;
            offset += len as u64;
        }
        self.commit()
    }

    /// Writes the memory's `size` bytes at `base`, a VM's that nothing changes meanwhile, as a
    /// cold-booted VM's snapshot into a memory cell does (TASK-456.4): the pages this process
    /// holds of it (`mincore`: resident, or on macOS compressed), all-zero ones discarded as
    /// holes; those it holds none of, untouched, which read as zeros, left as the holes they are.
    /// To be committed by [`MemoryCell::commit`].
    ///
    /// # Safety
    ///
    /// `base` is a page-aligned mapping of this process's of the memory's size, which nothing
    /// writes while this runs.
    pub unsafe fn write_resident(&self, base: NonNull<u8>) -> Result<(), Error> {
        let host = cladefs_core::pages::page_size();
        // A chunk's bits at a time, a byte a host page: 64 MiB of memory, 16 KiB of bits at most.
        const CHUNK: u64 = 64 << 20;
        // Resident, or (macOS's MINCORE_PAGED_OUT) compressed or paged out.
        #[cfg(target_os = "macos")]
        const HELD: u8 = 0x1 | 0x20;
        #[cfg(target_os = "linux")]
        const HELD: u8 = 0x1;
        let mut bits = vec![0u8; (CHUNK / host) as usize];
        let mut chunk = 0;
        while chunk < self.size {
            let len = (self.size - chunk).min(CHUNK);
            let bits = &mut bits[..(len / host) as usize];
            // SAFETY: Whole host pages of the mapping the caller gives, a byte for each.
            let ret = unsafe {
                libc::mincore(
                    base.as_ptr().add(chunk as usize).cast(),
                    len as usize,
                    bits.as_mut_ptr().cast(),
                )
            };
            if ret != 0 {
                return Err(Error::new(std::io::Error::last_os_error())
                    .context("asking which of the memory's pages are held"));
            }
            let mut page = 0;
            while page < bits.len() {
                if bits[page] & HELD == 0 {
                    page += 1;
                    continue;
                }
                let start = page;
                while page < bits.len() && bits[page] & HELD != 0 {
                    page += 1;
                }
                let at = chunk + start as u64 * host;
                let run = (page - start) as u64 * host;
                // SAFETY: Within the mapping, which nothing writes meanwhile, as the caller
                // promises.
                let bytes = unsafe {
                    std::slice::from_raw_parts(base.as_ptr().add(at as usize), run as usize)
                };
                self.write_sparse(at, bytes)?;
            }
            chunk += len;
        }
        Ok(())
    }

    /// Applies sparse diff memory files (as produced by Firecracker's `PUT /snapshot/create` with
    /// `snapshot_type: "Diff"`) on top of this memory cell in order (oldest to newest), committing
    /// them once. Must be called before [`MemoryCell::window`].
    ///
    /// Only allocated (non-hole) data ranges in each diff file are read and written (via
    /// [`MemoryCell::write_sparse`], so dirty zero pages become zero holes shadowing non-zero base
    /// pages); unallocated holes across all diff files stay inherited from the base memory cell
    /// version with zero copies. When multiple diff files touch the same offset, only the newest
    /// file's bytes are written.
    pub fn apply_diff_files(&self, diff_files: &[&Path]) -> Result<(), Error> {
        use std::io::{Read as _, Seek as _, SeekFrom};
        if self.window.lock().unwrap().is_some() {
            return Err(Error::new(Errno::EINVAL)
                .context("cannot apply diff files after the memory cell window is created"));
        }
        if diff_files.is_empty() {
            return Ok(());
        }
        // Validate every diff file up front before writing anything to the cell.
        let mut opened = Vec::with_capacity(diff_files.len());
        for path in diff_files {
            let file = host_fs::File::open(path).map_err(|err| {
                Error::new(err).context(format!("opening diff {}", path.display()))
            })?;
            let held = file.metadata().map_err(Error::new)?.len();
            if held != self.size {
                return Err(Error::new(Errno::EINVAL).context(format!(
                    "diff file {} is {held} bytes, not {}",
                    path.display(),
                    self.size
                )));
            }
            opened.push(file);
        }
        let page = self.page_size();
        // Collect page-aligned allocated spans from each diff file and resolve overlaps so later
        // diff files win and each modified page is written at most once.
        let mut spans: Vec<(usize, std::ops::Range<u64>)> = Vec::new();
        for (idx, file) in opened.iter().enumerate() {
            let ranges = host_fs::data_ranges(file, self.size).map_err(Error::new)?;
            for range in ranges {
                let start = (range.start / page) * page;
                let end = range.end.next_multiple_of(page).min(self.size);
                if start >= end {
                    continue;
                }
                spans = overlay_diff_span(spans, idx, start..end);
            }
        }
        if spans.is_empty() {
            return Ok(());
        }
        let mut buffer = vec![0u8; cladefs_core::memory::MEMORY_IMPORT_BYTES];
        for (idx, range) in spans {
            let file = &mut opened[idx];
            file.seek(SeekFrom::Start(range.start)).map_err(Error::new)?;
            let mut offset = range.start;
            while offset < range.end {
                let len = (range.end - offset).min(cladefs_core::memory::MEMORY_IMPORT_BYTES as u64)
                    as usize;
                let chunk = &mut buffer[..len];
                file.read_exact(chunk).map_err(Error::new)?;
                self.write_sparse(offset, chunk)?;
                offset += len as u64;
            }
        }
        self.commit()
    }

    /// Flushes all private pages in this memory cell's window into the cell (zero pages discarded
    /// as holes), commits them durably, and reclaims them back to the newly committed extents so
    /// no private pages remain.
    ///
    /// # Safety
    ///
    /// As [`MemoryCell::snapshot_window`].
    unsafe fn flush_window_private(&self, base: NonNull<u8>) -> Result<(), Error> {
        if self.window.lock().unwrap().is_none() {
            return Err(Error::new(Errno::EINVAL)
                .context("snapshotting memory of a memory cell with no window"));
        }
        let runs = self.private_runs()?;
        for run in &runs {
            let len = (run.end - run.start) as usize;
            // SAFETY: `run` is a page-aligned range within `0..self.size`, and `base` is the
            // window's mapping of `self.size` bytes whose private pages are readable and not
            // concurrently modified while the caller holds the VM paused.
            let bytes =
                unsafe { std::slice::from_raw_parts(base.as_ptr().add(run.start as usize), len) };
            self.write_sparse(run.start, bytes)?;
        }
        self.commit()?;
        for run in &runs {
            self.reclaim(run.start, run.end - run.start)?;
        }
        Ok(())
    }

    /// Snapshots the memory cell's window (TASK-456.4): writes its window's private pages into the
    /// cell (all-zero ones discarded as holes), commits them at once, reclaims them back to the
    /// newly committed extents so no private pages remain, and makes a version of the cell.
    ///
    /// # Safety
    ///
    /// `base` must be the base address previously passed to [`MemoryCell::window`], and no guest
    /// vCPU or other thread may be concurrently mutating the window's memory (e.g., the VM is
    /// paused).
    pub unsafe fn snapshot_window(&self, base: NonNull<u8>) -> Result<CellVersion, Error> {
        // SAFETY: Caller upholds the window base and quiescence invariant.
        unsafe { self.flush_window_private(base) }?;
        self.make_version()
    }

    /// Snapshots a host-local ephemeral memory cell's window in place: writes its private pages
    /// into the cell (zero pages discarded as holes), commits them, reclaims them so no private
    /// pages remain, and checkpoints the ephemeral cell durably on host SSD (punching freed runs)
    /// without making a bucket version or uploading anything to object storage.
    ///
    /// # Safety
    ///
    /// As [`MemoryCell::snapshot_window`].
    pub unsafe fn snapshot_window_ephemeral(
        &self,
        base: NonNull<u8>,
    ) -> Result<cladefs_core::CellId, Error> {
        if !self.is_ephemeral() {
            return Err(Error::new(Errno::EINVAL)
                .context("snapshot_window_ephemeral called on a non-ephemeral memory cell"));
        }
        // SAFETY: Caller upholds the window base and quiescence invariant.
        unsafe { self.flush_window_private(base) }?;
        self.client.snapshot_cell().map_err(Error::new)?;
        Ok(cladefs_core::CellId { tenant: self.tenant(), id: self.cell() })
    }

    /// Makes a version of the cell as of its last commit: a VM snapshot's memory, which any number
    /// of hosts restore at once ([`MemoryCell::open_forked`]), and none writes.
    pub fn make_version(&self) -> Result<CellVersion, Error> {
        self.client.make_version().map_err(Error::new)
    }

    /// The runs of its window's private pages, by offset in the memory, in order: those the
    /// guest stored to, its own, which its next snapshot writes.
    pub fn private_runs(&self) -> Result<Vec<std::ops::Range<u64>>, Error> {
        self.client.private_runs().map_err(Error::new)
    }

    /// How many private pages its window holds.
    pub fn private_pages(&self) -> Result<u64, Error> {
        let runs = self.private_runs()?;
        Ok(runs.iter().map(|run| (run.end - run.start) / self.page_size()).sum())
    }

    /// The extents the cell holds, read whole: those its image's last commit names, and those
    /// it has yet to free.
    pub fn extents(&self) -> Result<Vec<storage_device::ExtentId>, Error> {
        self.blocks.extents().map_err(Error::new)
    }

    /// Closes the cell, every commit durable already.
    pub fn close(self) -> Result<(), Error> {
        self.blocks.close().map_err(Error::new)
    }

    /// Makes the `size` bytes at `base`, the memory's own size, the cell's window: a VM's memory
    /// restored from it. See the module docs. `prefault`, if any, is told of the pages mapped for
    /// the guest ahead of its accesses.
    ///
    /// # Safety
    ///
    /// The memory must be a page-aligned mapping that nothing else in the process uses, and that
    /// outlives the cell.
    #[cfg(target_os = "linux")]
    pub unsafe fn window(
        &self,
        base: NonNull<u8>,
        size: u64,
        prefault: Option<Arc<dyn Prefault>>,
    ) -> Result<(), Error> {
        if size != self.size {
            return Err(Error::new(Errno::EINVAL)
                .context(format!("a window of {size} bytes over a memory of {}", self.size)));
        }
        cladefs_core::pages::window_fits_host(self.page_size(), cladefs_core::pages::page_size())?;
        let mut window = self.window.lock().unwrap();
        if window.is_some() {
            return Err(Error::new(Errno::EINVAL).context("the memory cell has a window already"));
        }
        // Round lazy-mapped chunks up to whole host pages so two adjacent chunks never straddle a
        // host page boundary (e.g. BlockDevice's 696,320-byte node on a 16 KiB host).
        let node_bytes =
            self.blocks.node_bytes().next_multiple_of(cladefs_core::pages::page_size());
        let words = size.div_ceil(node_bytes).div_ceil(64) as usize;
        let shared = Arc::new(Nodes {
            node_bytes,
            size,
            naming: Mutex::new(vec![0; words]),
            state: Mutex::new(NodesState { mapped: vec![0; words], waiting: HashMap::new() }),
        });
        let (asks, asked) = mpsc::channel();
        let mapper = Mapper {
            nodes: shared.clone(),
            blocks: Arc::downgrade(&self.blocks),
            client: Arc::downgrade(&self.client),
        };
        #[cfg(any(test, feature = "testing"))]
        fail::fail_point!("memory_cell::spawn_mapper", |_| Err(Errno::EAGAIN.into()));
        std::thread::Builder::new()
            .name("cladefs-memory-nodes".into())
            .spawn(move || mapper.serve(asked))
            .map_err(Error::new)?;
        let unmapped = Arc::new(Reach { nodes: shared.clone(), asks: Mutex::new(asks) });
        // SAFETY: As the caller promises.
        unsafe { self.client.create_memory_window_in_host_memory(base, size, prefault, unmapped) }?;
        *window = Some(shared);
        Ok(())
    }

    /// Reclaims the `len` bytes of its window at `offset`, whole pages, which the guest gave back,
    /// as a balloon's inflation or a free page report does: maps the image's runs there again, as
    /// its last commit has them, as the guest's `MAP` would, which ends the private pages there,
    /// their memory emptied, and names the image's pages there anew, which the guest reads next
    /// (see the module docs), none of them installed ahead, as a node's are: the guest touches a
    /// page it gave back only once it takes it back, and writes it first, as it allocates it. Only
    /// in the tree nodes the window has mapped: a node it hasn't, no fault reached yet, so the
    /// guest holds no private page there, and the mapper maps it whole as a fault first reaches
    /// it. `EINVAL` without a window, or for a range not whole pages within the memory.
    #[cfg(target_os = "linux")]
    pub fn reclaim(&self, offset: u64, len: u64) -> Result<(), Error> {
        self.whole(offset, len)?;
        let nodes = self.window.lock().unwrap().clone().ok_or_else(|| {
            Error::new(Errno::EINVAL).context("reclaiming memory of a memory cell with no window")
        })?;
        if len == 0 {
            return Ok(());
        }
        // Within the memory, as `whole` checked.
        let end = offset + len;
        // Order reclaim against naming, including a MAP_MANY that named only some of its runs.
        // The fault thread uses the separate state lock to reach a node while the RPC is pending.
        let naming = nodes.naming.lock().unwrap();
        let mut mapped: Vec<std::ops::Range<u64>> = Vec::new();
        for node in offset / nodes.node_bytes..=(end - 1) / nodes.node_bytes {
            if naming[(node / 64) as usize] & (1 << (node % 64)) == 0 {
                continue;
            }
            let start = (node * nodes.node_bytes).max(offset);
            let stop = ((node + 1) * nodes.node_bytes).min(end);
            match mapped.last_mut() {
                Some(range) if range.end == start => range.end = stop,
                _ => mapped.push(start..stop),
            }
        }
        let mut mappings = Vec::new();
        for range in mapped {
            mappings.extend(image_mappings(&self.blocks, range.start, range.end - range.start)?);
        }
        let result = self.client.map_many(&mappings).map_err(Error::new);
        drop(naming);
        result
    }

    /// cladefs: makes `ram`, the memory's own size, the cell's window, a guest's RAM on
    /// Hypervisor.framework, whose VMM's devices access it as the guest does (see the module
    /// docs): the image mapped there whole, as its last commit has it, before this returns, the
    /// guest's RAM not mapped yet. `EINVAL` for RAM of another size, or with a window already.
    #[cfg(target_os = "macos")]
    pub fn window_in_ram(&self, ram: Arc<dyn GuestRam>) -> Result<(), Error> {
        if ram.size() != self.size {
            return Err(Error::new(Errno::EINVAL).context(format!(
                "RAM of {} bytes over a memory of {}",
                ram.size(),
                self.size
            )));
        }
        cladefs_core::pages::window_fits_host(self.page_size(), cladefs_core::pages::page_size())?;
        let mut window = self.window.lock().unwrap();
        if window.is_some() {
            return Err(Error::new(Errno::EINVAL).context("the memory cell has a window already"));
        }
        let stage = self.client.create_memory_window_in_guest_ram(ram)?;
        self.map_image(&stage, 0, self.size, true)?;
        *window = Some(stage);
        Ok(())
    }

    /// cladefs: [`MemoryCell::reclaim`] on macOS: the image's pages mapped anew over the whole
    /// host pages of the `len` bytes at `offset` (those a host page straddles stay as they are),
    /// their private pages ended, their memory the host's again.
    #[cfg(target_os = "macos")]
    pub fn reclaim(&self, offset: u64, len: u64) -> Result<(), Error> {
        self.whole(offset, len)?;
        let stage = self.window.lock().unwrap().clone().ok_or_else(|| {
            Error::new(Errno::EINVAL).context("reclaiming memory of a memory cell with no window")
        })?;
        let host = cladefs_core::pages::page_size();
        let (start, end) = (offset.next_multiple_of(host), (offset + len) / host * host);
        if start >= end {
            return Ok(());
        }
        self.map_image(&stage, start, end - start, false)
    }

    /// Maps the image's `len` bytes at `start`, whole host pages, into the RAM `stage` is, as its
    /// last commit has them: what was there let go of first, the daemon's mappings and the RAM's
    /// private pages alike, unless the RAM is `fresh` from its window's making, its zeros, which
    /// the daemon maps nothing of; then the host pages of its holes alone left the RAM's zeros,
    /// the rest mapped by the daemon, each faulted in that it didn't map as it mapped them, so
    /// that every page is there as this returns.
    #[cfg(target_os = "macos")]
    fn map_image(
        &self,
        stage: &HvfRamStage,
        start: u64,
        len: u64,
        fresh: bool,
    ) -> Result<(), Error> {
        let host = cladefs_core::pages::page_size();
        if !fresh {
            // The daemon's mappings there go, ending the private pages it mapped over; then the
            // rest of them, those of holes it mapped nothing at, which a map there would keep.
            self.client.unmap(start, len).map_err(Error::new)?;
            stage.discard(start, len);
        }
        let mut mapped = Vec::new();
        for mapping in image_mappings(&self.blocks, start, len)? {
            let (from, to) = (mapping.window_offset, mapping.window_offset + mapping.len);
            // The host pages a hole covers whole are the RAM's zeros; its pieces of the others
            // the daemon composes with what is beside them.
            let (inner, outer) = (from.next_multiple_of(host), to / host * host);
            if !matches!(mapping.contents, MappingContents::Zeros) || inner >= outer {
                mapped.push(mapping);
                continue;
            }
            if from < inner {
                mapped.push(WindowMapping { window_offset: from, len: inner - from, ..mapping });
            }
            if outer < to {
                mapped.push(WindowMapping { window_offset: outer, len: to - outer, ..mapping });
            }
        }
        self.client.map_many_warm(&mapped).map_err(Error::new)?;
        let ranges: Vec<std::ops::Range<u64>> = (mapped.iter())
            .map(|mapping| mapping.window_offset..mapping.window_offset + mapping.len)
            .collect();
        for at in stage.unnamed(&ranges) {
            self.client.fault_in(at).map_err(|errno| {
                Error::new(errno).context(format!("faulting in the memory's page at {at:#x}"))
            })?;
        }
        match stage.unnamed(&ranges).first() {
            None => Ok(()),
            Some(at) => Err(Error::new(Errno::EIO)
                .context(format!("the daemon mapped nothing of the memory's page at {at:#x}"))),
        }
    }

    /// Fails with `EINVAL` unless `len` bytes at `offset` are whole pages within the memory.
    fn whole(&self, offset: u64, len: u64) -> Result<(), Error> {
        let page = self.page_size();
        let within = offset.checked_add(len).is_some_and(|end| end <= self.size);
        match offset.is_multiple_of(page) && len.is_multiple_of(page) && within {
            true => Ok(()),
            false => Err(Error::new(cladefs_core::Errno::EINVAL).context(format!(
                "{len} bytes at {offset}: not whole pages of {page} within {}",
                self.size
            ))),
        }
    }
}

/// The tree nodes of a memory's window, by the bytes each names: which the daemon has mapped, and
/// the faults waiting for those being mapped.
#[cfg(target_os = "linux")]
struct Nodes {
    node_bytes: u64,
    size: u64,
    /// A bit a node that might have named pages. Held across changes to the window by the
    /// mapper and reclaim, since pages become accessible before MAP_MANY returns.
    naming: Mutex<Vec<u64>>,
    state: Mutex<NodesState>,
}

#[cfg(target_os = "linux")]
struct NodesState {
    /// A bit a node: whether its runs are mapped.
    mapped: Vec<u64>,
    /// The faults waiting for each node being mapped.
    waiting: HashMap<u64, Vec<oneshot::Sender<Result<(), Errno>>>>,
}

#[cfg(target_os = "linux")]
impl NodesState {
    fn is_mapped(&self, node: u64) -> bool {
        self.mapped[(node / 64) as usize] & (1 << (node % 64)) != 0
    }
}

/// What a memory's window's faults reach first ([`Unmapped`]): a node mapped, at once; one not,
/// asked of the [`Mapper`], once however many faults wait for it.
#[cfg(target_os = "linux")]
struct Reach {
    nodes: Arc<Nodes>,
    asks: Mutex<mpsc::Sender<u64>>,
}

#[cfg(target_os = "linux")]
impl Unmapped for Reach {
    fn reach(&self, offset: u64) -> BoxFuture<'static, cladefs_core::Result<()>> {
        let node = offset / self.nodes.node_bytes;
        let mut state = self.nodes.state.lock().unwrap();
        if state.is_mapped(node) {
            return Box::pin(std::future::ready(Ok(())));
        }
        let (sender, receiver) = oneshot::channel();
        let waiting = state.waiting.entry(node).or_default();
        waiting.push(sender);
        if waiting.len() == 1 && self.asks.lock().unwrap().send(node).is_err() {
            // The mapper went with the cell: nothing maps the node.
            state.waiting.remove(&node);
            return Box::pin(std::future::ready(Err(Errno::EIO)));
        }
        drop(state);
        Box::pin(async move { receiver.await.unwrap_or(Err(Errno::EIO)) })
    }
}

/// Maps the nodes faults ask for, on a thread of its own, as reading the cell's tree blocks:
/// until the window's faults, and with them the [`Reach`], go.
#[cfg(target_os = "linux")]
struct Mapper {
    nodes: Arc<Nodes>,
    blocks: Weak<BlockDevice>,
    client: Weak<CellClient>,
}

#[cfg(target_os = "linux")]
impl Mapper {
    fn serve(self, asked: mpsc::Receiver<u64>) {
        for node in asked {
            // cladefs: a bug's panic mapping it, as a test has it (board 9414).
            #[cfg(test)]
            fail::fail_point!("memory_cell::mapping");
            let mapped = self.map(node);
            if let Err(error) = &mapped {
                log::error!("cladefs: mapping node {node} of a memory's window: {error:#}");
            }
            let result =
                mapped.as_ref().map(drop).map_err(|error| error.errno().unwrap_or(Errno::EIO));
            let mut state = self.nodes.state.lock().unwrap();
            if result.is_ok() {
                state.mapped[(node / 64) as usize] |= 1 << (node % 64);
            }
            for waiting in state.waiting.remove(&node).unwrap_or_default() {
                // Its waiter gone: nothing to tell.
                let _ = waiting.send(result);
            }
        }
    }

    /// Maps the runs of node `node` of the image, as its last commit has them, and installs its
    /// pages ahead of the guest's touches: `MAP` with `MAP_WARM` warms each mapped
    /// extent range in the daemon's extent cache directly (no throwaway IPC plaintext copy,
    /// board 11681) before installing the mappings, and `prefault` installs the cached pages into
    /// the window. A page installed takes a load of the guest's no fault here (see the module docs).
    fn map(&self, node: u64) -> Result<(), Error> {
        let gone = || Error::new(Errno::EIO).context("the memory cell went");
        let (blocks, client) =
            (self.blocks.upgrade().ok_or_else(gone)?, self.client.upgrade().ok_or_else(gone)?);
        let start = node * self.nodes.node_bytes;
        let len = self.nodes.node_bytes.min(self.nodes.size - start);
        let mappings = image_mappings(&blocks, start, len)?;
        {
            let mut naming = self.nodes.naming.lock().unwrap();
            naming[(node / 64) as usize] |= 1 << (node % 64);
            client.map_many_warm(&mappings).map_err(Error::new)?;
        }
        client.prefault(start, len);
        Ok(())
    }
}

/// cladefs: a mapper gone, its window's faults gone, or a bug's panic ending its thread (board
/// 9414): the faults waiting for the nodes it was to map fail (`EIO`), as those after it do,
/// finding it gone ([`Reach`]). Before, a panic left them waiting for ever, and every fault after
/// on a node one waited for, their guest's vCPUs with them.
#[cfg(target_os = "linux")]
impl Drop for Mapper {
    fn drop(&mut self) {
        // What a panic holding it left: the faults waiting, which go, and nodes mapped, as they
        // were, a node's bit set once its pages are.
        self.nodes.state.clear_poison();
        let mut state = self.nodes.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // Each sender dropped, its fault fails.
        state.waiting.clear();
    }
}

/// Layers `(file_idx, new_range)` on top of sorted, non-overlapping `existing` spans so that the
/// newer `file_idx` overwrites any overlapped sub-ranges while preserving sorted non-overlapping
/// order (coalescing adjacent spans from the same `file_idx`).
fn overlay_diff_span(
    existing: Vec<(usize, std::ops::Range<u64>)>,
    file_idx: usize,
    new: std::ops::Range<u64>,
) -> Vec<(usize, std::ops::Range<u64>)> {
    let mut out: Vec<(usize, std::ops::Range<u64>)> = Vec::with_capacity(existing.len() + 2);
    let mut push = |idx: usize, range: std::ops::Range<u64>| {
        if range.start >= range.end {
            return;
        }
        if let Some((last_idx, last_range)) = out.last_mut()
            && *last_idx == idx
            && last_range.end == range.start
        {
            last_range.end = range.end;
            return;
        }
        out.push((idx, range));
    };
    let mut inserted = false;
    for (idx, range) in existing {
        if range.end <= new.start {
            push(idx, range);
        } else if range.start >= new.end {
            if !inserted {
                push(file_idx, new.clone());
                inserted = true;
            }
            push(idx, range);
        } else {
            // Overlap: keep any prefix before `new.start`.
            if range.start < new.start {
                push(idx, range.start..new.start);
            }
            if !inserted {
                push(file_idx, new.clone());
                inserted = true;
            }
            // Keep any suffix after `new.end`.
            if range.end > new.end {
                push(idx, new.end..range.end);
            }
        }
    }
    if !inserted {
        push(file_idx, new);
    }
    out
}

/// The window's mappings of the `len` bytes of the image at `start`, as its last commit has
/// them: its runs of data, each its extent's, and of holes, zeros.
fn image_mappings(blocks: &BlockDevice, start: u64, len: u64) -> Result<Vec<WindowMapping>, Error> {
    let runs = blocks.runs(start, len).map_err(Error::new)?;
    Ok((runs.into_iter())
        .filter_map(|(range, contents)| {
            let contents = match contents? {
                Contents::Extent { extent, offset } => MappingContents::Extent {
                    extent: cladefs_core::ExtentId { cell: extent.cell, sequence: extent.sequence },
                    offset,
                },
                Contents::Zeros => MappingContents::Zeros,
            };
            Some(WindowMapping {
                window_offset: range.start,
                len: range.end - range.start,
                contents,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use super::*;
    #[cfg(target_os = "linux")]
    use cladefs_core::pages::page_size;

    /// board 9414: the faults waiting for a memory's window's mapper that panicked fail (`EIO`),
    /// and those after it: before, they waited for ever, their senders kept in the nodes' state.
    #[cfg(target_os = "linux")]
    #[test]
    fn faults_waiting_for_a_mapper_that_panicked_fail() {
        let scenario = fail::FailScenario::setup();
        fail::cfg("memory_cell::mapping", "panic").unwrap();
        let node_bytes = page_size();
        let nodes = Arc::new(Nodes {
            node_bytes,
            size: 4 * node_bytes,
            naming: Mutex::new(vec![0]),
            state: Mutex::new(NodesState { mapped: vec![0], waiting: HashMap::new() }),
        });
        let (asks, asked) = mpsc::channel();
        let mapper = Mapper { nodes: nodes.clone(), blocks: Weak::new(), client: Weak::new() };
        let serving = std::thread::spawn(move || mapper.serve(asked));
        let reach = Arc::new(Reach { nodes, asks: Mutex::new(asks) });
        let fault = |offset: u64| {
            let (done, result) = mpsc::channel();
            let reaching = reach.reach(offset);
            std::thread::spawn(move || done.send(futures::executor::block_on(reaching)).unwrap());
            result.recv_timeout(std::time::Duration::from_secs(10)).expect("the fault waits")
        };
        assert_eq!(fault(0), Err(Errno::EIO));
        assert!(serving.join().is_err(), "the mapper panicked");
        assert_eq!(fault(node_bytes), Err(Errno::EIO));
        scenario.teardown();
    }

    #[test]
    fn runs_of_zeros_split_pages_of_zeros_from_the_rest() {
        use cladefs_core::memory::{PageRun, runs_of_zeros};
        let mut bytes = vec![0; 4 * 8];
        bytes[9] = 1;
        bytes[17] = 2;
        let shape: Vec<_> = (runs_of_zeros(&bytes, 4).into_iter())
            .map(|(at, run)| match run {
                PageRun::Zeros(len) => (at, len, true),
                PageRun::Data(data) => (at, data.len(), false),
            })
            .collect();
        assert_eq!(
            shape,
            vec![(0, 8, true), (8, 4, false), (12, 4, true), (16, 4, false), (20, 12, true)]
        );
    }
}
