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

use crate::error::{Errno, Error};
use crate::{
    CellClient, CellVersion, DaemonConnection, DaemonDevice, MappingContents, Prefault, Unmapped,
    WindowMapping,
};
use block_device::{BlockDevice, Contents, Options};
use futures::channel::oneshot;
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, Weak, mpsc};
use storage_device::DeviceHolder;

/// The cell capacity a memory of `size` bytes takes at most: its pages, the tree naming them (a
/// node of 170 refs a page) and their owners (8 bytes a page), and room for a commit's worth of
/// pages written again besides its compactions.
pub fn capacity(size: u64) -> u64 {
    crate::import::memory_capacity(size)
}

/// A VM's memory, a cell of its own. See the module docs.
pub struct MemoryCell {
    client: Arc<CellClient>,
    blocks: Arc<BlockDevice>,
    size: u64,
    /// Its window's tree nodes, once it has one ([`MemoryCell::window`]).
    window: Mutex<Option<Arc<Nodes>>>,
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
    pub fn open_cell(socket: &Path, cell: crate::CellId, size: u64) -> Result<Self, Error> {
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
        expected: Option<crate::CellId>,
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
                return Err(Error::new(crate::Errno::EINVAL)
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
            false => Err(Error::new(crate::Errno::EINVAL).context(format!(
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
        for (at, run) in crate::import::runs_of_zeros(bytes, page) {
            let at = offset + at as u64;
            match run {
                crate::import::PageRun::Zeros(len) => self.discard(at, len as u64)?,
                crate::import::PageRun::Data(data) => self.write(at, data)?,
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
        let mut buffer = vec![0; crate::import::MEMORY_IMPORT_BYTES];
        let mut offset = 0;
        while offset < self.size {
            let len = (self.size - offset).min(crate::import::MEMORY_IMPORT_BYTES as u64) as usize;
            let chunk = &mut buffer[..len];
            file.read_exact(chunk).map_err(Error::new)?;
            self.write_sparse(offset, chunk)?;
            offset += len as u64;
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
    ) -> Result<crate::CellId, Error> {
        if !self.is_ephemeral() {
            return Err(Error::new(Errno::EINVAL)
                .context("snapshot_window_ephemeral called on a non-ephemeral memory cell"));
        }
        // SAFETY: Caller upholds the window base and quiescence invariant.
        unsafe { self.flush_window_private(base) }?;
        self.client.snapshot_cell().map_err(Error::new)?;
        Ok(crate::CellId { tenant: self.tenant(), id: self.cell() })
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
        crate::pages::window_fits_host(self.page_size(), crate::pages::page_size())?;
        let mut window = self.window.lock().unwrap();
        if window.is_some() {
            return Err(Error::new(Errno::EINVAL).context("the memory cell has a window already"));
        }
        // Round lazy-mapped chunks up to whole host pages so two adjacent chunks never straddle a
        // host page boundary (e.g. BlockDevice's 696,320-byte node on a 16 KiB host).
        let node_bytes = self.blocks.node_bytes().next_multiple_of(crate::pages::page_size());
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
        #[cfg(test)]
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

/// The tree nodes of a memory's window, by the bytes each names: which the daemon has mapped, and
/// the faults waiting for those being mapped.
struct Nodes {
    node_bytes: u64,
    size: u64,
    /// A bit a node that might have named pages. Held across changes to the window by the
    /// mapper and reclaim, since pages become accessible before MAP_MANY returns.
    naming: Mutex<Vec<u64>>,
    state: Mutex<NodesState>,
}

struct NodesState {
    /// A bit a node: whether its runs are mapped.
    mapped: Vec<u64>,
    /// The faults waiting for each node being mapped.
    waiting: HashMap<u64, Vec<oneshot::Sender<Result<(), Errno>>>>,
}

impl NodesState {
    fn is_mapped(&self, node: u64) -> bool {
        self.mapped[(node / 64) as usize] & (1 << (node % 64)) != 0
    }
}

/// What a memory's window's faults reach first ([`Unmapped`]): a node mapped, at once; one not,
/// asked of the [`Mapper`], once however many faults wait for it.
struct Reach {
    nodes: Arc<Nodes>,
    asks: Mutex<mpsc::Sender<u64>>,
}

impl Unmapped for Reach {
    fn reach(&self, offset: u64) -> BoxFuture<'static, crate::Result<()>> {
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
struct Mapper {
    nodes: Arc<Nodes>,
    blocks: Weak<BlockDevice>,
    client: Weak<CellClient>,
}

impl Mapper {
    fn serve(self, asked: mpsc::Receiver<u64>) {
        for node in asked {
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
    /// pages ahead of the guest's touches: its data read through the cell first, so that the
    /// daemon holds its pages, which only then the mapping names, and the window installs. A page
    /// installed takes a load of the guest's no fault here (see the module docs).
    fn map(&self, node: u64) -> Result<(), Error> {
        let gone = || Error::new(Errno::EIO).context("the memory cell went");
        let (blocks, client) =
            (self.blocks.upgrade().ok_or_else(gone)?, self.client.upgrade().ok_or_else(gone)?);
        let start = node * self.nodes.node_bytes;
        let len = self.nodes.node_bytes.min(self.nodes.size - start);
        let mut read = vec![0; len as usize];
        blocks.read(start, &mut read).map_err(Error::new)?;
        let mappings = image_mappings(&blocks, start, len)?;
        {
            let mut naming = self.nodes.naming.lock().unwrap();
            naming[(node / 64) as usize] |= 1 << (node % 64);
            client.map_many(&mappings).map_err(Error::new)?;
        }
        client.prefault(start, len);
        Ok(())
    }
}

/// The window's mappings of the `len` bytes of the image at `start`, as its last commit has
/// them: its runs of data, each its extent's, and of holes, zeros.
fn image_mappings(blocks: &BlockDevice, start: u64, len: u64) -> Result<Vec<WindowMapping>, Error> {
    let runs = blocks.runs(start, len).map_err(Error::new)?;
    Ok((runs.into_iter())
        .filter_map(|(range, contents)| {
            let contents = match contents? {
                Contents::Extent { extent, offset } => MappingContents::Extent {
                    extent: crate::ExtentId { cell: extent.cell, sequence: extent.sequence },
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
    use super::*;
    use crate::pages::page_size;
    use std::os::unix::fs::MetadataExt as _;

    /// `len` bytes of `byte` plus their offset modulo 251.
    fn pattern(byte: u8, len: usize) -> Vec<u8> {
        (0..len).map(|i| byte ^ (i % 251) as u8).collect()
    }

    /// The bytes of host memory a window's private pages hold: its private memory's blocks, the
    /// one memory object of this process named so of `size` bytes.
    fn private_memory_held(size: u64) -> u64 {
        let held: Vec<u64> = (std::fs::read_dir("/proc/self/fd").unwrap())
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                // A descriptor closed since it was listed: not the private memory's.
                let link = std::fs::read_link(&path).ok()?;
                let named = link.to_string_lossy().starts_with("/memfd:cladefs-private-pages");
                let metadata = std::fs::metadata(&path).ok().filter(|_| named)?;
                (metadata.len() == size).then(|| metadata.blocks() * 512)
            })
            .collect();
        assert_eq!(held.len(), 1, "a window's private memory: {held:?}");
        held[0]
    }

    /// Pauses the mapper after it names and installs a node's pages and before it wakes
    /// the faults waiting for the node. A write fault can use an already named page meanwhile.
    struct PausePrefault {
        first: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
    }

    impl Prefault for PausePrefault {
        fn prefault(&self, _: u64, _: u64) {
            let first = self.first.lock().unwrap().take();
            if let Some((entered, resume)) = first {
                entered.send(()).unwrap();
                resume.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
            }
        }
    }

    // board 8497: a window's pages the guest gave back, a balloon's inflation's, reclaimed, are
    // private no more, their memory emptied, the host's again: the guest reads the image there
    // again, as of the cell's last commit, and its next write makes a page private anew. A node no
    // fault reached is left to its first fault, which maps it whole. Discarded with madvise, as
    // Firecracker's balloon discarded them, they stayed private, their memory held.
    #[test]
    fn reclaimed_pages_end_private_and_read_the_image() {
        // SAFETY: nextest runs each test in a process of its own, whose other threads don't read
        // the environment yet.
        unsafe {
            std::env::set_var("CLADEFSD_BUCKET", "memory:///cells");
            std::env::set_var("CLADEFSD_TOKEN", "1");
        }
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("cladefsd.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || crate::cladefsd::server::serve(listener));
        let ps = page_size();
        // Three nodes of the tree's, the last part of one.
        const PAGES: u64 = 400;
        let size = PAGES * ps;
        let memory = MemoryCell::open(&socket, &dir.path().join("memory"), size).unwrap();
        let image: Vec<u8> = (0..PAGES).flat_map(|page| pattern(page as u8, ps as usize)).collect();
        memory.write(0, &image).unwrap();
        memory.commit().unwrap();
        let version = memory.make_version().unwrap();
        let page = memory.page_size();
        memory.close().unwrap();
        let fork = dir.path().join("fork");
        let memory_cell = MemoryCell::open_forked(&socket, &fork, version, size).unwrap();
        let reclaimed = memory_cell.reclaim(0, ps);
        assert_eq!(reclaimed.unwrap_err().errno(), Some(Errno::EINVAL), "with no window");
        // SAFETY: Anonymous memory, reserving none, the test's: the window's.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        assert_ne!(base, libc::MAP_FAILED);
        let base = NonNull::new(base.cast::<u8>()).unwrap();
        let (entered, named) = mpsc::channel();
        let (finished, resume) = mpsc::channel();
        let prefault = Arc::new(PausePrefault { first: Mutex::new(Some((entered, resume))) });
        // A failure starting the mapper leaves the cell with no window and permits a retry.
        fail::cfg("memory_cell::spawn_mapper", "1*return").unwrap();
        // SAFETY: The memory is the test's and outlives the memory cell.
        let failed = unsafe { memory_cell.window(base, size, None) };
        fail::remove("memory_cell::spawn_mapper");
        assert_eq!(failed.unwrap_err().errno(), Some(Errno::EAGAIN));
        let reclaimed = memory_cell.reclaim(0, ps);
        assert_eq!(reclaimed.unwrap_err().errno(), Some(Errno::EINVAL));
        // SAFETY: The memory is the test's, and outlives the memory_cell (unmapped once it goes).
        unsafe { memory_cell.window(base, size, Some(prefault)) }.unwrap();
        let address = base.as_ptr() as usize;
        let at = |page: u64| address + (page * ps) as usize;
        // The window's bytes of pages `pages`, as the guest reads them.
        let read = |pages: std::ops::Range<u64>| -> Vec<u8> {
            // SAFETY: The window's memory, whose faults its memory_cell serves.
            (at(pages.start)..at(pages.end))
                .map(|addr| unsafe { (addr as *const u8).read_volatile() })
                .collect()
        };
        let image_of = |pages: std::ops::Range<u64>| {
            image[(pages.start * ps) as usize..(pages.end * ps) as usize].to_vec()
        };
        crate::guest_thread_started();
        let store = |page: u64, byte: u8| {
            // SAFETY: As above, a write of the guest's.
            unsafe { (at(page) as *mut u8).write_volatile(byte) }
        };
        std::thread::scope(|threads| {
            let first = threads.spawn(|| read(0..1));
            named.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
            // This store copies locally because the mapper has already named the page.
            store(5, 0xee);
            let private = memory_cell.private_runs();
            let reclaimed = memory_cell.reclaim(5 * ps, ps);
            let remaining = memory_cell.private_runs();
            finished.send(()).unwrap();
            assert_eq!(first.join().unwrap(), image_of(0..1));
            assert_eq!(private.unwrap(), vec![5 * ps..6 * ps]);
            reclaimed.unwrap();
            assert!(remaining.unwrap().is_empty(), "reclaim skipped a named page");
        });
        // The first two nodes reached, the last not.
        assert!(read(0..340) == image_of(0..340), "the window holds the image");
        for page in [5, 6, 7, 300] {
            store(page, 0xee);
        }
        assert_eq!(memory_cell.private_runs().unwrap(), vec![5 * ps..8 * ps, 300 * ps..301 * ps]);
        assert_eq!(private_memory_held(size), 4 * ps);

        memory_cell.reclaim(4 * ps, 4 * ps).unwrap();
        assert_eq!(memory_cell.private_runs().unwrap(), vec![300 * ps..301 * ps]);
        assert_eq!(private_memory_held(size), ps);
        assert!(read(4..8) == image_of(4..8), "the pages reclaimed read the image");
        store(6, 0xdd);
        assert_eq!(memory_cell.private_runs().unwrap(), vec![6 * ps..7 * ps, 300 * ps..301 * ps]);
        assert_eq!(read(6..7)[0], 0xdd);

        // Over the last node, which no fault reached, and a page of the one before it.
        memory_cell.reclaim(300 * ps, 100 * ps).unwrap();
        assert_eq!(memory_cell.private_runs().unwrap(), vec![6 * ps..7 * ps]);
        assert_eq!(private_memory_held(size), ps);
        assert!(read(300..400) == image_of(300..400), "the last node reads the image");

        // On hosts with pages larger than the cell's (e.g. 16 KiB arm64), reclaiming one cell page
        // inside a private host page restores only that cell page while preserving sibling private
        // cell pages in the same host page; a subsequent write to the reclaimed cell page re-tracks
        // COW ownership so later sibling reclaims never clobber it (review 100 finding 342).
        if ps > page {
            memory_cell.reclaim(6 * ps + page, page).unwrap();
            assert_eq!(memory_cell.private_runs().unwrap(), vec![6 * ps..7 * ps]);
            assert_eq!(read(6..7)[0], 0xdd, "sibling cell page stayed private");
            assert_eq!(
                &read(6..7)[page as usize..(2 * page) as usize],
                &image_of(6..7)[page as usize..(2 * page) as usize],
                "reclaimed cell page reads the image again"
            );
            // Write into the reclaimed cell page (slot 1), then reclaim sibling slot 2: slot 1's
            // new write must survive rather than being overwritten by old image bytes.
            // SAFETY: Within the window's live mapping, a write of the guest's.
            unsafe { ((at(6) + page as usize) as *mut u8).write_volatile(0x55) };
            if ps >= 3 * page {
                memory_cell.reclaim(6 * ps + 2 * page, page).unwrap();
                assert_eq!(read(6..7)[0], 0xdd, "slot 0 stayed private");
                assert_eq!(
                    read(6..7)[page as usize],
                    0x55,
                    "write to previously reclaimed slot 1 survived sibling reclaim"
                );
                assert_eq!(
                    &read(6..7)[(2 * page) as usize..(3 * page) as usize],
                    &image_of(6..7)[(2 * page) as usize..(3 * page) as usize],
                    "reclaimed slot 2 reads the image again"
                );
            }
        }

        let reclaimed = memory_cell.reclaim(page, page / 2);
        assert_eq!(reclaimed.unwrap_err().errno(), Some(Errno::EINVAL), "half a page");
        let reclaimed = memory_cell.reclaim(399 * ps, 2 * ps);
        assert_eq!(reclaimed.unwrap_err().errno(), Some(Errno::EINVAL), "past the memory");
        memory_cell.close().unwrap();
        // SAFETY: Unmaps the memory, which nothing uses any more.
        assert_eq!(unsafe { libc::munmap(base.as_ptr().cast(), size as usize) }, 0);
    }

    #[test]
    fn runs_of_zeros_split_pages_of_zeros_from_the_rest() {
        use crate::import::{PageRun, runs_of_zeros};
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

    // TASK-456.4: snapshotting a memory cell's window writes only its private pages (zeroed ones
    // discarded as holes), commits, reclaims them back to the newly committed extents so no
    // private pages remain, and supports taking another snapshot after resuming writes.
    #[test]
    fn snapshot_window_writes_private_pages_reclaims_and_supports_subsequent_snapshots() {
        // SAFETY: nextest runs each test in a process of its own, whose other threads don't read
        // the environment yet.
        unsafe {
            std::env::set_var("CLADEFSD_BUCKET", "memory:///cells");
            std::env::set_var("CLADEFSD_TOKEN", "1");
        }
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("cladefsd.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || crate::cladefsd::server::serve(listener));
        let ps = page_size();
        const PAGES: u64 = 32;
        let size = PAGES * ps;
        let base_cell = MemoryCell::open(&socket, &dir.path().join("base"), size).unwrap();
        let mut expected: Vec<u8> =
            (0..PAGES).flat_map(|page| pattern(page as u8, ps as usize)).collect();
        base_cell.write(0, &expected).unwrap();
        base_cell.commit().unwrap();
        let v1 = base_cell.make_version().unwrap();
        let page = base_cell.page_size();
        base_cell.close().unwrap();

        let fork = dir.path().join("fork");
        let memory_cell = MemoryCell::open_forked(&socket, &fork, v1, size).unwrap();
        // SAFETY: Anonymous memory, reserving none, the test's: the window's.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        assert_ne!(base, libc::MAP_FAILED);
        let base = NonNull::new(base.cast::<u8>()).unwrap();
        // SAFETY: The memory is the test's, and outlives `memory_cell`.
        unsafe { memory_cell.window(base, size, None) }.unwrap();
        let per_host_page = ps / page;
        crate::guest_thread_started();
        let write_page = |p: u64, data: &[u8]| {
            // SAFETY: Within the window's mapping of `size` bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    data.as_ptr(),
                    base.as_ptr().add((p * ps) as usize),
                    ps as usize,
                );
            }
        };
        // Modify page 2 with new data, and zero page 5 completely (should become a hole).
        let page2 = pattern(0x99, ps as usize);
        let zeros = vec![0u8; ps as usize];
        write_page(2, &page2);
        write_page(5, &zeros);
        expected[2 * ps as usize..3 * ps as usize].copy_from_slice(&page2);
        expected[5 * ps as usize..6 * ps as usize].copy_from_slice(&zeros);
        assert_eq!(memory_cell.private_pages().unwrap(), 2 * per_host_page);

        // SAFETY: `base` is the window's mapping and no other thread touches it.
        let v2 = unsafe { memory_cell.snapshot_window(base) }.unwrap();
        assert_eq!(memory_cell.private_pages().unwrap(), 0, "private pages reclaimed after snap");
        assert_eq!(private_memory_held(size), 0, "private memfd emptied after snap");

        // Verify a child forked from v2 reads the exact snapshot bytes (and still matches expected).
        let child1 =
            MemoryCell::open_forked(&socket, &dir.path().join("child1"), v2, size).unwrap();
        let mut got = vec![0u8; size as usize];
        child1.read(0, &mut got).unwrap();
        assert!(got == expected, "v2 bytes match");
        child1.close().unwrap();

        // Resume writing on the same window, then take a second snapshot v3.
        let page9 = pattern(0x42, ps as usize);
        write_page(9, &page9);
        expected[9 * ps as usize..10 * ps as usize].copy_from_slice(&page9);
        assert_eq!(memory_cell.private_pages().unwrap(), per_host_page);
        // SAFETY: `base` is the window's mapping and no other thread touches it.
        let v3 = unsafe { memory_cell.snapshot_window(base) }.unwrap();
        assert_eq!(memory_cell.private_pages().unwrap(), 0);

        let child2 =
            MemoryCell::open_forked(&socket, &dir.path().join("child2"), v3, size).unwrap();
        child2.read(0, &mut got).unwrap();
        assert!(got == expected, "v3 bytes match");
        child2.close().unwrap();

        memory_cell.close().unwrap();
        // SAFETY: Unmaps the test's memory after `memory_cell` dropped.
        assert_eq!(unsafe { libc::munmap(base.as_ptr().cast(), size as usize) }, 0);
    }

    #[test]
    fn ephemeral_memory_cell_suspends_resumes_resuspends_and_deletes_without_bucket_uploads() {
        // SAFETY: nextest runs each test in a process of its own, whose other threads don't read
        // the environment yet.
        unsafe {
            std::env::set_var("CLADEFSD_BUCKET", "memory:///ephemeral-memory-cells");
            std::env::set_var("CLADEFSD_TOKEN", "1");
        }
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("cladefsd.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || crate::cladefsd::server::serve(listener));
        let ps = page_size();
        const PAGES: u64 = 32;
        let size = PAGES * ps;
        let cell_dir = dir.path().join("ephemeral-mem");

        // Opening missing ephemeral memory cell fails with ENOENT.
        assert_eq!(
            MemoryCell::open_ephemeral_existing(&socket, &cell_dir, None, size)
                .err()
                .and_then(|err| err.errno()),
            Some(Errno::ENOENT)
        );

        // Cold-boot suspend: create standalone ephemeral memory cell and populate guest RAM.
        let mut expected: Vec<u8> =
            (0..PAGES).flat_map(|page| pattern(page as u8, ps as usize)).collect();
        let cold = MemoryCell::open_ephemeral(&socket, &cell_dir, size).unwrap();
        assert!(cold.is_ephemeral());
        assert_eq!(cold.make_version().unwrap_err().errno(), Some(Errno::ENOTSUP));
        let cell_id = crate::CellId { tenant: cold.tenant(), id: cold.cell() };
        let page = cold.page_size();
        // Record baseline after any one-time BucketKms tenant KEK creation (`Upload::Other`);
        // cell data (`Segments`/`Extents`/`Checkpoints`) must stay strictly zero, and zero
        // further bytes of any kind may be uploaded during writes, commits, re-suspend, or delete.
        let bucket = crate::cladefsd::cells::daemon_bucket().unwrap();
        let cell_data_uploads = |b: &bucket_cell::object_bucket::RoutedBucket| -> u64 {
            let stats = b.stats();
            stats.uploaded(bucket_cell::object_bucket::Upload::Segments)
                + stats.uploaded(bucket_cell::object_bucket::Upload::Extents)
                + stats.uploaded(bucket_cell::object_bucket::Upload::Checkpoints)
        };
        let uploaded_bytes = |b: &bucket_cell::object_bucket::RoutedBucket| -> u64 {
            let stats = b.stats();
            bucket_cell::object_bucket::Upload::ALL
                .into_iter()
                .map(|kind| stats.uploaded(kind))
                .sum()
        };
        let baseline_uploads = uploaded_bytes(&bucket);
        assert_eq!(cell_data_uploads(&bucket), 0, "ephemeral create must upload no cell data");

        cold.write_sparse(0, &expected).unwrap();
        cold.commit().unwrap();
        cold.close().unwrap();

        assert_eq!(cell_data_uploads(&bucket), 0, "ephemeral memory cell must upload no cell data");
        assert_eq!(
            uploaded_bytes(&bucket),
            baseline_uploads,
            "ephemeral memory cell write/commit/close must upload nothing to bucket"
        );

        // Mismatched expected CellId is rejected with EEXIST.
        let wrong_id = crate::CellId { tenant: cell_id.tenant, id: cell_id.id + 1 };
        assert_eq!(
            MemoryCell::open_ephemeral_existing(&socket, &cell_dir, Some(wrong_id), size)
                .err()
                .and_then(|err| err.errno()),
            Some(Errno::EEXIST)
        );

        // Resume in place via open_ephemeral_existing + uffd window.
        let resumed =
            MemoryCell::open_ephemeral_existing(&socket, &cell_dir, Some(cell_id), size).unwrap();
        assert!(resumed.is_ephemeral());
        // SAFETY: Anonymous memory, reserving none, the test's: the window's.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        assert_ne!(base, libc::MAP_FAILED);
        let base = NonNull::new(base.cast::<u8>()).unwrap();
        // SAFETY: The memory is the test's and outlives `resumed`.
        unsafe { resumed.window(base, size, None) }.unwrap();
        crate::guest_thread_started();
        let per_host_page = ps / page;
        // Read page 0 lazily via userfaultfd and verify contents.
        // SAFETY: Within the window's mapping of `size` bytes.
        let page0: Vec<u8> =
            (0..ps as usize).map(|i| unsafe { base.as_ptr().add(i).read_volatile() }).collect();
        assert_eq!(page0, &expected[..ps as usize]);

        // Modify page 3 with new data and zero page 7 completely (hole).
        let page3 = pattern(0xab, ps as usize);
        let zeros = vec![0u8; ps as usize];
        // SAFETY: Within the window's mapping of `size` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(
                page3.as_ptr(),
                base.as_ptr().add((3 * ps) as usize),
                ps as usize,
            );
            std::ptr::copy_nonoverlapping(
                zeros.as_ptr(),
                base.as_ptr().add((7 * ps) as usize),
                ps as usize,
            );
        }
        expected[3 * ps as usize..4 * ps as usize].copy_from_slice(&page3);
        expected[7 * ps as usize..8 * ps as usize].copy_from_slice(&zeros);
        assert_eq!(resumed.private_pages().unwrap(), 2 * per_host_page);

        // Re-suspend in place without creating any bucket version.
        // SAFETY: `base` is the window's mapping and no other thread touches it.
        let snapped = unsafe { resumed.snapshot_window_ephemeral(base) }.unwrap();
        assert_eq!(snapped, cell_id);
        assert_eq!(resumed.private_pages().unwrap(), 0);
        assert_eq!(private_memory_held(size), 0);
        resumed.close().unwrap();
        // SAFETY: Unmaps the test's memory after `resumed` dropped.
        assert_eq!(unsafe { libc::munmap(base.as_ptr().cast(), size as usize) }, 0);

        // Reopen again and verify every single byte persisted across re-suspend.
        let reopened =
            MemoryCell::open_ephemeral_existing(&socket, &cell_dir, Some(cell_id), size).unwrap();
        let mut got = vec![0u8; size as usize];
        reopened.read(0, &mut got).unwrap();
        assert!(got == expected, "re-suspended ephemeral memory cell bytes differ");
        reopened.close().unwrap();

        // Still zero bucket uploads across cold-boot suspend, resume, re-suspend, and reopen.
        assert_eq!(cell_data_uploads(&bucket), 0, "ephemeral memory cell uploaded cell data");
        assert_eq!(
            uploaded_bytes(&bucket),
            baseline_uploads,
            "ephemeral memory cell uploaded bytes to bucket"
        );

        // Deleting a standalone ephemeral memory cell succeeds without CLADEFSD_EPHEMERAL.
        // Wait for the daemon thread's asynchronous close of `reopened` to release the cell
        // directory claim before deleting directly (board 12317).
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(
            async {
                local_cell::unclaimed_on(crate::Host::process().local(), &cell_dir).await.unwrap();
                crate::OpenCell::delete_on(crate::Host::process(), &cell_dir, Some(&bucket))
                    .await
                    .unwrap();
            },
        );
        assert!(!cell_dir.exists());
        assert_eq!(
            uploaded_bytes(&bucket),
            baseline_uploads,
            "deleting standalone ephemeral memory cell must upload nothing to bucket"
        );
    }
}
