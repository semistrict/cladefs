// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A block device on an extent device: one array of bytes, read and written at offsets, its
//! blocks discarded, resized, and flushed, each flush one commit of the extent device. What a
//! SQLite cell holds (its database, sqlite_cladefs) and a memory cell (a VM's memory, TASK-456).
//!
//! **Flushes.** A flush ([`BlockDevice::flush`]) writes the plan's extents, then the root, naming
//! the tree as the flush leaves it, committed with the extent device's `commit_root`, a durable
//! barrier. So a crash leaves the contents as of a flush. A flush that fails before its root
//! leaves the contents as they were, the extents it wrote freed; one that fails committing the
//! root fails the block device, which may or may not hold it, until it is opened again.
//!
//! **Freeing.** What a flush leaves unreferenced is freed once a root naming it as dying is
//! durable, so that the next opening frees it if a crash came between: [`DYING_KEPT`] of them at a
//! time, a request for many rather than one a flush, the roots naming them meanwhile. A root holds
//! few (the extent daemon's is 4 KiB at most): more, as a flush that compacts, shrinks or discards
//! much leaves, are listed in an extent of their own, written with the flush, which the root
//! names, and which is freed a flush later. The root also says where minting goes on: the extents
//! past it a crash left, written for a flush whose root it lost, are freed by the next opening, as
//! Fxfs's allocator frees its orphans.
//!
//! **The window.** A block device opened with a window ([`BlockDevice::open_mapped`]) maps its
//! contents into it from offset 0, as far as the window goes: what reads and fetches reach, as
//! the last flush has it (`window`), as they first reach it (TASK-440); and maps again what
//! flushes changed, a batch at a time ([`REMAP_BATCH`]), or as pages are fetched: reads of what
//! they changed go to the extent device meanwhile. Reads of the rest copy from the window, and
//! fetches ([`BlockDevice::fetch`]) read it in place, as SQLite's memory-mapped reads do. While
//! pages fetched are held, nothing is mapped again, as SQLite's own unix VFS remaps nothing while
//! pages are out. A map that fails leaves what it was for unmapped, read from the extent device
//! until the next read or fetch reaching it maps it again, and is counted
//! ([`BlockDevice::map_failures`]): the window is never given up on for good (board 8549).
//!
//! **Opening** reads the root alone: the tree is read as it is used (`pages`, TASK-440).
//!
//! I/O to the extent device blocks the calling thread ([`futures::executor::block_on`]), as
//! SQLite's calls do: a block device is not to be called from a thread whose executor the extent
//! device needs.

use crate::format::{self, DYING_KEPT, MAX_LISTED, MAX_ROOT_BYTES, Root};
use crate::pages::{Failure, Pages, Plan, REMAP_BATCH};
use crate::window::{Extents, Run, Window};
use anyhow::{Context as _, Error, bail};
use futures::executor::block_on;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use storage_device::{Device, DeviceHolder, ExtentId, MAX_ROOT};

/// The most sequence numbers an opening frees from either end of what a crash may have left
/// (Fxfs's `MAX_ORPHANS_SOUGHT`).
const MAX_ORPHANS_SOUGHT: u64 = 1 << 16;

/// The most extents freed in one request.
const FREES_AT_ONCE: usize = 4096;

// Every root a flush writes fits an extent device's.
const _: () = assert!(MAX_ROOT_BYTES <= MAX_ROOT);

/// The error of using a block device whose flush failed as it committed its root: whether the
/// extent device holds that flush, its next opening finds.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the block device failed: {0}")]
pub struct Failed(pub String);

/// How a block device lays out what it writes.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// The most blocks an extent holds: a flush writing more writes several.
    pub max_extent_blocks: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self { max_extent_blocks: 1024 }
    }
}

/// What a block device holds, under its lock.
struct Inner {
    pages: Pages,
    /// The sequence number the next extent minted gets.
    mint: u64,
    /// Extents left unreferenced whose frees failed, or have not been made yet.
    dying: Vec<ExtentId>,
    /// The extent listing the dying extents the last root holds too many of to name itself, which
    /// the next opening reads: dying once a root that names it not is durable, as the next
    /// flush's (TASK-540).
    list: Option<ExtentId>,
    failed: Option<String>,
    /// How many pages are held, fetched ([`BlockDevice::fetch`]).
    fetches: u32,
}

/// A block device on an extent device. See the module docs.
pub struct BlockDevice {
    /// Dropped before the window, as [`Window`] promises its windows.
    device: DeviceHolder,
    options: Options,
    inner: Mutex<Inner>,
    /// The window, if any, and the bytes of it the contents are mapped into: whole blocks.
    window: Option<(Box<dyn Window>, u64)>,
    /// How many times pages were fetched to read in place.
    fetched: AtomicU64,
    /// How many times mapping the window failed.
    map_failures: AtomicU64,
}

/// Frees `extents` on `device`, a request at most per [`FREES_AT_ONCE`]: those it failed to free.
async fn free(device: &dyn Device, extents: &[ExtentId]) -> Vec<ExtentId> {
    let mut failed = Vec::new();
    for batch in extents.chunks(FREES_AT_ONCE) {
        for (extent, result) in batch.iter().zip(device.free_extents(batch).await) {
            if let Err(error) = result {
                log::warn!("block_device: freeing {extent:?}: {error:#}");
                failed.push(*extent);
            }
        }
    }
    failed
}

/// The sequence numbers between `minted` and `next` a crash may have left extents at: all of them,
/// or the first and last [`MAX_ORPHANS_SOUGHT`], as minting starts past a floor that may be far
/// past `minted`.
fn orphans(minted: u64, next: u64) -> impl Iterator<Item = u64> {
    let first = minted..minted.saturating_add(MAX_ORPHANS_SOUGHT).min(next.max(minted));
    let last = next.saturating_sub(MAX_ORPHANS_SOUGHT).max(first.end)..next;
    first.chain(last)
}

impl BlockDevice {
    /// The block device on `device`, made, of no bytes, if the device's root is empty.
    pub fn open(device: DeviceHolder, options: Options) -> Result<Arc<Self>, Error> {
        assert!(options.max_extent_blocks > 0, "extents of no blocks");
        let inner = block_on(Self::recover(&**device))?;
        Ok(Self::made(device, options, inner, None))
    }

    fn made(
        device: DeviceHolder,
        options: Options,
        inner: Inner,
        window: Option<(Box<dyn Window>, u64)>,
    ) -> Arc<Self> {
        let (fetched, map_failures) = (AtomicU64::new(0), AtomicU64::new(0));
        let inner = Mutex::new(inner);
        Arc::new(Self { device, options, inner, window, fetched, map_failures })
    }

    /// The block device on `device`, as [`BlockDevice::open`] opens it, its contents mapped into
    /// `window` from offset 0, as far as the window's whole blocks go. Failing, it drops `device`
    /// before `window`, as [`Window`] promises.
    pub fn open_mapped(
        device: DeviceHolder,
        window: Box<dyn Window>,
        options: Options,
    ) -> Result<Arc<Self>, Error> {
        assert!(options.max_extent_blocks > 0, "extents of no blocks");
        let block_size = device.block_size() as u64;
        let (base, size) = window.memory();
        let bytes = size / block_size * block_size;
        let inner = match (base.as_ptr() as u64).is_multiple_of(block_size) && bytes > 0 {
            true => block_on(Self::recover(&**device)),
            false => {
                Err(anyhow::anyhow!("a window of {size} bytes at {base:?} holds no aligned block"))
            }
        };
        let mut inner = match inner {
            Ok(inner) => inner,
            Err(error) => {
                drop(device);
                drop(window);
                return Err(error);
            }
        };
        inner.pages.set_region(Some((base, bytes)));
        Ok(Self::made(device, options, inner, Some((window, bytes))))
    }

    /// What `device` holds, its root's dying extents and what a crash left freed, and the root
    /// made if it was empty.
    async fn recover(device: &dyn Device) -> Result<Inner, Error> {
        let block_size = device.block_size() as usize;
        format::check_block_size(block_size)?;
        let root = Root::decode(&device.root().await.context("reading the root")?)?;
        let next = device.next_sequence().await?;
        let floor = device.sequence_floor().await?;
        let mut inner = Inner {
            pages: Pages::new(block_size),
            mint: next.max(floor),
            dying: Vec::new(),
            list: None,
            failed: None,
            fetches: 0,
        };
        let Some(root) = root else {
            let made = Root { minted: inner.mint, ..Root::default() };
            device.commit_root(&made.encode()).await.context("making the block device")?;
            return Ok(inner);
        };
        inner.mint = inner.mint.max(root.minted);
        inner.pages = Pages::open(block_size, &root.tree).context("opening the tree")?;
        let cell = device.cell();
        let orphans = orphans(root.minted, next).map(|sequence| ExtentId::new(cell, sequence));
        let mut freeing: Vec<_> = root.dying.iter().copied().chain(orphans).collect();
        if let Some((list, count)) = root.dying_list {
            let len = format::list_len(count, block_size).context("the dying extents' list")?;
            let mut buffer = device.allocate_buffer(len).await;
            device
                .read(list, 0, buffer.as_mut())
                .await
                .context("reading the dying extents' list")?;
            freeing.extend(format::decode_list(&buffer.as_ref().to_vec(), count)?);
        }
        if !freeing.is_empty() {
            inner.dying = free(device, &freeing).await;
            // Sealed before anything is written past them.
            device.flush().await.context("sealing the frees of what a crash left")?;
        }
        inner.list = root.dying_list.map(|(list, _)| list);
        Ok(inner)
    }

    /// Closes the extent device: every flush is durable already, and the extent device may keep
    /// them elsewhere too, as an image uploads them to its bucket, which closing it waits for.
    /// What was written since the last flush is lost.
    pub fn close(&self) -> Result<(), Error> {
        let mut inner = self.lock()?;
        if inner.pages.changed() {
            log::warn!("block_device: closed with changes not flushed, which are lost");
        }
        // What waits to be freed, which the last root names, so that a crash before the extent
        // device's closing seals the frees frees them again; not the list it names, which the
        // next opening reads.
        let dying = std::mem::take(&mut inner.dying);
        inner.dying = block_on(free(&**self.device, &dying));
        block_on(self.device.close()).context("closing the device")
    }

    /// The extent device.
    pub fn device(&self) -> &DeviceHolder {
        &self.device
    }

    /// The bytes of a block, which discards and runs are whole ones of.
    pub fn block_size(&self) -> u64 {
        self.device.block_size() as u64
    }

    fn lock(&self) -> Result<MutexGuard<'_, Inner>, Error> {
        let inner = self.inner.lock().unwrap();
        if let Some(failed) = &inner.failed {
            bail!(Failed(failed.clone()));
        }
        Ok(inner)
    }

    /// The extents it holds, in order: those its last flush references, the tree read whole for
    /// it, and those it has yet to free.
    pub fn extents(&self) -> Result<Vec<ExtentId>, Error> {
        let inner = self.lock()?;
        let mut extents: Vec<_> = inner.dying.iter().chain(&inner.list).copied().collect();
        extents.extend(block_on(inner.pages.extents(&**self.device))?);
        extents.sort();
        Ok(extents)
    }

    /// Maps `changed`, ranges of chunks, into the window as the last flush has them. A failure
    /// leaves them unmapped, read from the extent device until a read or fetch reaching them maps
    /// them again, and is logged and counted ([`BlockDevice::map_failures`]): the window stays, as
    /// pages held, fetched, read it in place (TASK-553), and no failure gives it up on for good
    /// (board 8549).
    fn remap(&self, inner: &mut Inner, changed: impl IntoIterator<Item = Range<u64>>) {
        let Some((window, bytes)) = self.window.as_ref() else { return };
        let block_size = self.block_size();
        let mapped = bytes / block_size;
        let changed: Vec<_> = changed
            .into_iter()
            .map(|range| range.start.min(mapped)..range.end.min(mapped))
            .collect();
        let (mut runs, mut unmapped, mut marked) = (Vec::new(), Vec::new(), Vec::new());
        let device = &**self.device;
        let made = changed.iter().try_for_each(|range| {
            for (chunks, contents) in block_on(inner.pages.runs(device, range.clone()))? {
                let (at, len) =
                    (chunks.start * block_size, (chunks.end - chunks.start) * block_size);
                match contents {
                    Some(contents) => runs.push(Run { at, len, contents }),
                    None => unmapped.push((at, len)),
                }
                marked.push((chunks, contents.is_some()));
            }
            Ok::<_, Error>(())
        });
        let made = made
            .and_then(|()| window.map(device, &runs))
            .and_then(|()| unmapped.iter().try_for_each(|(at, len)| window.unmap(*at, *len)));
        match made {
            Ok(()) => inner.pages.mark_mapped(marked),
            Err(error) => {
                self.map_failures.fetch_add(1, Ordering::Relaxed);
                log::error!(
                    "block_device: mapping the window, read from the device until mapped again: \
                     {error:#}"
                );
                // What it was for, and what it mapped of it before it failed, read from the
                // device: the chunks it took as stale among them, which the window may map as
                // before the flushes that changed them.
                inner.pages.mark_mapped(changed.into_iter().map(|range| (range, false)));
            }
        }
    }

    /// Maps the chunks flushes changed since the window last mapped them, as the last has them:
    /// unless pages are held, fetched, which read as they were until given back, as SQLite's own
    /// unix VFS remaps nothing while pages are out.
    fn refresh(&self, inner: &mut Inner) {
        if self.window.is_some() && inner.fetches == 0 && inner.pages.stale_chunks() > 0 {
            let stale = inner.pages.take_stale();
            self.remap(inner, stale);
        }
    }

    /// Maps what the window doesn't yet of the chunks `len` bytes at `offset` reach, as the last
    /// flush has them.
    fn map_reached(&self, inner: &mut Inner, offset: u64, len: u64) {
        if self.window.is_none() || len == 0 {
            return;
        }
        let block_size = self.block_size();
        let unmapped =
            inner.pages.unmapped(offset / block_size..(offset + len).div_ceil(block_size));
        if !unmapped.is_empty() {
            self.remap(inner, unmapped);
        }
    }

    /// Where the `len` bytes at `offset` are in the window, if it maps them as they read: to read
    /// in place until given back ([`BlockDevice::unfetch`]).
    pub fn fetch(&self, offset: u64, len: u64) -> Result<Option<*const u8>, Error> {
        let mut inner = self.lock()?;
        let inner = &mut *inner;
        if self.window.is_none() {
            return Ok(None);
        }
        self.refresh(inner);
        self.map_reached(inner, offset, len);
        let fetched = block_on(inner.pages.fetch(&**self.device, offset, len))?;
        inner.fetches += fetched.is_some() as u32;
        self.fetched.fetch_add(fetched.is_some() as u64, Ordering::Relaxed);
        Ok(fetched)
    }

    /// Whether its contents are mapped into a window ([`BlockDevice::open_mapped`]), which reads
    /// in place ([`BlockDevice::fetch`]) need.
    pub fn is_mapped(&self) -> bool {
        self.window.is_some()
    }

    /// How many times pages were fetched to read in place ([`BlockDevice::fetch`]).
    pub fn fetched(&self) -> u64 {
        self.fetched.load(Ordering::Relaxed)
    }

    /// How many times mapping the window failed: each left what it was for read from the extent
    /// device until mapped again.
    pub fn map_failures(&self) -> u64 {
        self.map_failures.load(Ordering::Relaxed)
    }

    /// Gives back a page fetched ([`BlockDevice::fetch`]).
    pub fn unfetch(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.fetches = inner.fetches.checked_sub(1).expect("a page fetched");
    }

    /// Reads into `buf` from `offset`, as far as the contents go: how many bytes it read.
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        let mut inner = self.lock()?;
        let inner = &mut *inner;
        self.map_reached(inner, offset, buf.len() as u64);
        block_on(inner.pages.read(&**self.device, offset, buf))
    }

    /// Writes `data` at `offset`, growing the contents if it ends past them.
    pub fn write(&self, offset: u64, data: &[u8]) -> Result<(), Error> {
        let mut inner = self.lock()?;
        block_on(inner.pages.write(&**self.device, offset, data))
    }

    /// The bytes of the contents one node of the tree names: what [`BlockDevice::runs`] of them
    /// reads one node for, and the blocks around it maps, once the nodes above it are read.
    pub fn node_bytes(&self) -> u64 {
        let block_size = self.device.block_size() as usize;
        (format::fanout(block_size) * block_size) as u64
    }

    /// What the contents hold of the `len` bytes at `offset`, whole blocks, as of the last flush:
    /// runs of bytes, each blocks of one extent from an offset in it, zeros (holes), or nothing
    /// (past the end), the tree read as far as they need. For what maps the contents into memory
    /// of its own, as a VM's memory window does (TASK-456.3), not through the block device's
    /// window.
    pub fn runs(&self, offset: u64, len: u64) -> Result<Extents, Error> {
        let block_size = self.block_size();
        if !offset.is_multiple_of(block_size) || !len.is_multiple_of(block_size) {
            bail!("the runs of {len} bytes at {offset}, not whole blocks of {block_size}");
        }
        let mut inner = self.lock()?;
        let chunks = offset / block_size..(offset + len) / block_size;
        let runs = block_on(inner.pages.runs(&**self.device, chunks))?;
        let bytes = |chunks: Range<u64>| chunks.start * block_size..chunks.end * block_size;
        Ok(runs.into_iter().map(|(chunks, contents)| (bytes(chunks), contents)).collect())
    }

    /// Discards the `len` bytes at `offset`, whole blocks: they read as zeros, and the next flush
    /// stores nothing for them (TASK-456.2).
    pub fn discard(&self, offset: u64, len: u64) -> Result<(), Error> {
        let block_size = self.block_size();
        if !offset.is_multiple_of(block_size) || !len.is_multiple_of(block_size) {
            bail!("discarding {len} bytes at {offset}, not whole blocks of {block_size}");
        }
        let mut inner = self.lock()?;
        let chunks = offset / block_size..(offset + len) / block_size;
        block_on(inner.pages.discard(&**self.device, chunks))
    }

    /// Makes the contents `size` bytes: what was past it, if they were longer, is gone, and reads
    /// as zeros if they grow again.
    pub fn resize(&self, size: u64) -> Result<(), Error> {
        let mut inner = self.lock()?;
        block_on(inner.pages.resize(&**self.device, size))
    }

    /// The contents' bytes, as written.
    pub fn size(&self) -> Result<u64, Error> {
        Ok(self.lock()?.pages.size())
    }

    /// Commits what changed since the last flush, durably: nothing if nothing did.
    pub fn flush(&self) -> Result<(), Error> {
        let mut inner = self.lock()?;
        let inner = &mut *inner;
        if !inner.pages.changed() {
            return Ok(());
        }
        let device = &**self.device;
        let (cell, mut next) = (device.cell(), inner.mint);
        let mint = || {
            next += 1;
            ExtentId::new(cell, next - 1)
        };
        let plan = block_on(inner.pages.plan(device, mint, self.options.max_extent_blocks));
        // What it minted is spent, whatever came of it.
        inner.mint = next;
        let plan = plan?;
        self.commit_root(inner, &plan).context("flushing the block device")?;
        inner.pages.apply(plan);
        // Mapped again a batch at a time: read from memory or the extent device meanwhile.
        if inner.pages.stale_chunks() >= REMAP_BATCH {
            self.refresh(inner);
        }
        Ok(())
    }

    /// Commits the root naming `plan`'s tree: the plan's extents written first, the last write
    /// carrying the root; and the plan's dead among the dying, freed once a batch of them waits
    /// ([`DYING_KEPT`]), their root durable. A failure before the root changes nothing, but that
    /// what it wrote is freed; one committing it fails the block device.
    fn commit_root(&self, inner: &mut Inner, plan: &Plan) -> Result<(), Error> {
        let device = &**self.device;
        // The last root's list among them: the root this commits names it not.
        let carried = std::mem::take(&mut inner.dying);
        let named = inner.list.take();
        let dying: Vec<_> =
            carried.iter().copied().chain(named).chain(plan.dead.iter().copied()).collect();
        // More than a list holds, which no root read back may name (board 8549): nothing written.
        if dying.len() as u64 > MAX_LISTED {
            (inner.dying, inner.list) = (carried, named);
            bail!("{} extents dying, past the {MAX_LISTED} a list holds", dying.len());
        }
        let mut root = Root { dying: dying.clone(), tree: plan.root.clone(), ..Root::default() };
        // Too many dying extents for the root, listed in an extent of their own, written with it.
        let mut list = None;
        if dying.len() > DYING_KEPT {
            let extent = ExtentId::new(device.cell(), inner.mint);
            inner.mint += 1;
            (root.dying, root.dying_list) = (Vec::new(), Some((extent, dying.len() as u64)));
            list = Some((extent, format::encode_list(&dying, device.block_size() as usize)));
        }
        root.minted = inner.mint;
        let encoded = root.encode();
        let written = block_on(async {
            if let Some((extent, bytes)) = &list {
                let mut buffer = device.allocate_buffer(bytes.len()).await;
                buffer.as_mut().copy_from_slice(bytes);
                let listed = device.write(*extent, 0, buffer.as_ref()).await;
                listed.context("writing the dying extents' list").map_err(Failure::Writing)?;
            }
            plan.write(device, Some(&encoded)).await
        });
        match written {
            Ok(()) => {}
            Err(Failure::Writing(error)) => {
                // Nothing names what it wrote: freed, or, failing that, dying.
                let wrote: Vec<_> =
                    list.iter().map(|(extent, _)| *extent).chain(plan.extents()).collect();
                (inner.dying, inner.list) = (carried, named);
                inner.dying.extend(block_on(free(device, &wrote)));
                return Err(error);
            }
            Err(Failure::Committing(error)) => {
                let error = error.context("committing the block device's root");
                inner.failed = Some(format!("{error:#}"));
                return Err(error);
            }
        }
        inner.dying = match list.is_some() || dying.len() >= DYING_KEPT {
            true => block_on(free(device, &dying)),
            false => dying,
        };
        inner.list = list.map(|(extent, _)| extent);
        Ok(())
    }

    /// Fails unless the block device is usable: what its users see as an error of their own.
    pub fn check(&self) -> Result<(), Error> {
        self.lock().map(drop)
    }
}

impl std::fmt::Debug for BlockDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.lock().map_err(|_| std::fmt::Error)?;
        f.debug_struct("BlockDevice")
            .field("size", &inner.pages.size())
            .field("mint", &inner.mint)
            .field("failed", &inner.failed)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window::Contents;
    use std::sync::atomic::AtomicU64;
    use storage_device::fake_device::{FakeDevice, Op};

    const BLOCK: usize = 512;

    /// The runs of the contents as of the last flush: their blocks' extents, zeros where they were
    /// discarded, nothing past their end; what was written since is not among them (TASK-456.3).
    #[test]
    fn runs_say_what_the_last_flush_holds() {
        let device = FakeDevice::new(1 << 20, BLOCK as u32).calm();
        let blocks = BlockDevice::open(DeviceHolder::new(device), Options::default()).unwrap();
        blocks.write(0, &vec![1; BLOCK * 8]).unwrap();
        blocks.discard(2 * BLOCK as u64, 2 * BLOCK as u64).unwrap();
        blocks.flush().unwrap();
        blocks.write(0, &vec![2; BLOCK]).unwrap();
        let block = BLOCK as u64;
        let runs = blocks.runs(0, 10 * block).unwrap();
        let shape: Vec<_> = (runs.iter())
            .map(|(range, contents)| {
                (range.clone(), contents.map(|c| matches!(c, Contents::Zeros)))
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                (0..2 * block, Some(false)),
                (2 * block..4 * block, Some(true)),
                (4 * block..8 * block, Some(false)),
                (8 * block..10 * block, None),
            ]
        );
        // The flushed bytes, not those written since.
        let Some(Contents::Extent { extent, offset }) = runs[0].1 else { panic!("an extent") };
        let device = blocks.device();
        let mut buffer = block_on(device.allocate_buffer(BLOCK));
        block_on(device.read(extent, offset, buffer.as_mut())).unwrap();
        let mut bytes = vec![0; BLOCK];
        buffer.as_ref().copy_to_slice(&mut bytes);
        assert_eq!(bytes, vec![1; BLOCK]);
        let unaligned = blocks.runs(1, block).unwrap_err().to_string();
        assert_eq!(unaligned, "the runs of 512 bytes at 1, not whole blocks of 512");
    }

    /// A block device's opening reads its root and none of its tree; a read of a page reads the
    /// nodes on its path and the page, and the device holds those nodes alone (TASK-440).
    #[test]
    fn an_opening_reads_no_tree_and_a_read_one_path() {
        let mut device = FakeDevice::new(1 << 20, BLOCK as u32).calm();
        let reads = Arc::new(AtomicU64::new(0));
        let counted = reads.clone();
        device.set_op_callback(move |op| {
            if matches!(op, Op::Read) {
                counted.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        });
        let blocks = BlockDevice::open(DeviceHolder::new(device), Options::default()).unwrap();
        blocks.write(0, &vec![3; BLOCK * 401]).unwrap();
        blocks.flush().unwrap();

        let before = reads.load(Ordering::Relaxed);
        let mut inner = block_on(BlockDevice::recover(&***blocks.device())).unwrap();
        // The root, which the fake device counts as a read, alone.
        assert_eq!(reads.load(Ordering::Relaxed) - before, 1, "reads at the opening");
        assert_eq!((inner.pages.root().height, inner.pages.nodes_held()), (3, 0));
        let mut page = vec![0; BLOCK];
        block_on(inner.pages.read(&***blocks.device(), BLOCK as u64 * 250, &mut page)).unwrap();
        assert_eq!(page, vec![3; BLOCK]);
        assert_eq!(reads.load(Ordering::Relaxed) - before, 1 + 4, "reads of one page");
        assert_eq!(inner.pages.nodes_held(), 3);
    }
}
