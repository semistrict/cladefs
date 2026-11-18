// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A VM's memory as a cell of its own (TASK-456.2): a store the host's extent daemon makes as
//! a memory cell (`OPEN_MEMORY`), kept as an `extent_files` file set of one file, [`IMAGE`], the
//! image of the guest's memory, a page a block. A snapshot writes the pages its guest wrote and
//! makes the zero ones holes, which store nothing, and commits them at once ([`MemoryCell`]); a
//! restored VM's next snapshot goes into a fork of its parent's memory ([`MemoryCell::fork`]),
//! which reads the parent's pages and stores only its own.
//!
//! Its device is the daemon's store ([`DaemonDevice`]): its reads of sealed extents are the
//! daemon's cache's, which every store and VM it serves shares.
//!
//! **Restored** (TASK-456.3), a VM's memory is the cell's window ([`MemoryCell::window`]): the
//! daemon's pages of its extents, shared with every VM restored from the same snapshot on the host,
//! mapped read-only, a page the guest stores to made a private copy of its own, as a DAX window
//! copies on write (backlog doc-13). The window maps nothing at first: a fault on a page of a tree
//! node not mapped yet maps that node's runs first ([`Unmapped`]), on a thread of the cell's own,
//! as reading the tree blocks, every fault on the node waiting for that one mapping; so a restore
//! reads the cell's root alone, and what of its tree the guest's touches reach. The node's pages
//! are installed with it, ahead of the guest's touches: KVM resolves a load of a page installed on
//! the vCPU, with no fault here, where a page not installed it hands to x86's asynchronous page
//! fault worker, whose fault is a write, a load's too, which the window can only take for a store,
//! a private copy (docs/research/kvm-async-pf-and-shared-guest-memory.md). So every write to the
//! window is a store, and a node's first page alone may be made private for a load. Its private
//! pages are the guest's memory, reserved nowhere in the daemon: the VM process's cgroup limits
//! them.

use crate::error::{Errno, Error};
use crate::{
    CellClient, CellVersion, DaemonConnection, DaemonDevice, MappingContents, Prefault, Unmapped,
    WindowMapping,
};
use extent_files::{Contents, FileSet, Options};
use futures::channel::oneshot;
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, Weak, mpsc};
use storage_device::DeviceHolder;

/// The file of a memory cell that is the guest's memory.
pub const IMAGE: &str = "memory";

/// The store capacity a memory of `size` bytes takes at most: its pages, the tree naming them (a
/// node of 170 refs a page) and their owners (8 bytes a page), and room for a commit's worth of
/// pages written again besides its compactions.
pub fn capacity(size: u64) -> u64 {
    size + size / 128 + size / 512 + (64 << 20)
}

/// A VM's memory, a cell of its own. See the module docs.
pub struct MemoryCell {
    store: Arc<CellClient>,
    files: Arc<FileSet>,
    size: u64,
}

impl MemoryCell {
    /// The memory cell at `store`, which the daemon listening at `socket` serves, made, a
    /// memory of `size` bytes of zeros, if it doesn't exist: `EINVAL` for one of another size.
    pub fn open(socket: &Path, store: &Path, size: u64) -> Result<Self, Error> {
        let daemon = DaemonConnection::connect(socket)?;
        Self::over(daemon.open_memory(store, capacity(size))?, size)
    }

    /// The memory cell at `store`, forked from the version `from` of a memory cell if it doesn't
    /// exist: a VM's memory restored from a snapshot, on any host, as its disks are
    /// (docs/integrator-guide.md). `EINVAL` for a memory of another size.
    pub fn open_forked(
        socket: &Path,
        store: &Path,
        from: CellVersion,
        size: u64,
    ) -> Result<Self, Error> {
        let daemon = DaemonConnection::connect(socket)?;
        Self::over(daemon.open_forked(store, from, capacity(size))?, size)
    }

    /// The memory cell `opened` is, of a memory of `size` bytes, made one of zeros if new.
    fn over(opened: CellClient, size: u64) -> Result<Self, Error> {
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

    /// The store's cell.
    pub fn cell(&self) -> u64 {
        self.store.cell()
    }

    /// Writes `pages` at `offset`, whole pages, to be committed by [`MemoryCell::commit`].
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
        let page = self.page_size() as usize;
        let mut buffer = vec![0; IMPORT_BYTES];
        let mut offset = 0;
        while offset < self.size {
            let len = (self.size - offset).min(IMPORT_BYTES as u64) as usize;
            let chunk = &mut buffer[..len];
            file.read_exact(chunk).map_err(Error::new)?;
            for (at, run) in runs_of_zeros(chunk, page) {
                let at = offset + at as u64;
                match run {
                    Run::Zeros(len) => self.punch(at, len as u64)?,
                    Run::Data(bytes) => self.write(at, bytes)?,
                }
            }
            offset += len as u64;
        }
        self.commit()
    }

    /// Makes a version of the cell as of its last commit: a VM snapshot's memory, which any number
    /// of hosts restore at once ([`MemoryCell::open_forked`]), and none writes.
    pub fn make_version(&self) -> Result<CellVersion, Error> {
        self.store.make_version().map_err(Error::new)
    }

    /// Forks the cell, as of its last commit, to a new store at `path`, a memory cell of a
    /// new cell, which reads its pages until it writes its own: its cell.
    pub fn fork(&self, path: &Path) -> Result<u64, Error> {
        self.store.fork(path).map_err(Error::new)
    }

    /// The runs of its window's private pages, by offset in the memory, in order: those the
    /// guest stored to, its own, which its next snapshot writes.
    pub fn private_runs(&self) -> Result<Vec<std::ops::Range<u64>>, Error> {
        self.store.private_runs().map_err(Error::new)
    }

    /// How many private pages its window holds.
    pub fn private_pages(&self) -> Result<u64, Error> {
        let runs = self.private_runs()?;
        Ok(runs.iter().map(|run| (run.end - run.start) / self.page_size()).sum())
    }

    /// The extents the cell holds, read whole: those its image's last commit names, and those
    /// it has yet to free.
    pub fn extents(&self) -> Result<Vec<storage_device::ExtentId>, Error> {
        self.files.extents().map_err(Error::new)
    }

    /// Closes the cell, every commit durable already.
    pub fn close(self) -> Result<(), Error> {
        self.files.close_file(IMAGE);
        self.files.close().map_err(Error::new)
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
        let node_bytes = self.files.node_bytes();
        let nodes = size.div_ceil(node_bytes);
        let shared = Arc::new(Nodes {
            node_bytes,
            size,
            state: Mutex::new(NodesState {
                mapped: vec![0; nodes.div_ceil(64) as usize],
                waiting: HashMap::new(),
            }),
        });
        let (asks, asked) = mpsc::channel();
        let mapper = Mapper {
            nodes: shared.clone(),
            files: Arc::downgrade(&self.files),
            store: Arc::downgrade(&self.store),
        };
        std::thread::Builder::new()
            .name("cladefs-memory-nodes".into())
            .spawn(move || mapper.serve(asked))
            .map_err(Error::new)?;
        let unmapped = Arc::new(Reach { nodes: shared, asks: Mutex::new(asks) });
        // SAFETY: As the caller promises.
        unsafe { self.store.create_memory_window_in_host_memory(base, size, prefault, unmapped) }
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

/// The bytes [`MemoryCell::import`] reads of a file at once.
const IMPORT_BYTES: usize = 16 << 20;

/// A run of pages of a buffer: all zeros, so many bytes of them, or not, these bytes.
enum Run<'a> {
    Zeros(usize),
    Data(&'a [u8]),
}

/// The runs of `page`-byte pages of `bytes`, whole pages, by their offsets in it.
fn runs_of_zeros(bytes: &[u8], page: usize) -> Vec<(usize, Run<'_>)> {
    let mut runs: Vec<(usize, Run<'_>)> = Vec::new();
    for (index, chunk) in bytes.chunks(page).enumerate() {
        let at = index * page;
        let zeros = chunk.iter().all(|byte| *byte == 0);
        match (runs.last_mut(), zeros) {
            (Some((_, Run::Zeros(len))), true) => *len += chunk.len(),
            (Some((start, Run::Data(data))), false) => *data = &bytes[*start..at + chunk.len()],
            (_, true) => runs.push((at, Run::Zeros(chunk.len()))),
            (_, false) => runs.push((at, Run::Data(chunk))),
        }
    }
    runs
}

/// The tree nodes of a memory's window, by the bytes each names: which the daemon has mapped, and
/// the faults waiting for those being mapped.
struct Nodes {
    node_bytes: u64,
    size: u64,
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
    files: Weak<FileSet>,
    store: Weak<CellClient>,
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
        let (files, store) =
            (self.files.upgrade().ok_or_else(gone)?, self.store.upgrade().ok_or_else(gone)?);
        let start = node * self.nodes.node_bytes;
        let len = self.nodes.node_bytes.min(self.nodes.size - start);
        let mut read = vec![0; len as usize];
        files.read(IMAGE, start, &mut read).map_err(Error::new)?;
        let runs = files.runs(IMAGE, start, len).map_err(Error::new)?;
        let mappings: Vec<WindowMapping> = (runs.into_iter())
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
            .collect();
        store.map_many(&mappings).map_err(Error::new)?;
        store.prefault(start, len);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_of_zeros_split_pages_of_zeros_from_the_rest() {
        let mut bytes = vec![0; 4 * 8];
        bytes[9] = 1;
        bytes[17] = 2;
        let shape: Vec<_> = (runs_of_zeros(&bytes, 4).into_iter())
            .map(|(at, run)| match run {
                Run::Zeros(len) => (at, len, true),
                Run::Data(data) => (at, data.len(), false),
            })
            .collect();
        assert_eq!(
            shape,
            vec![(0, 8, true), (8, 4, false), (12, 4, true), (16, 4, false), (20, 12, true)]
        );
    }
}
