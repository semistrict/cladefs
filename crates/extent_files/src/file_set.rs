// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A file set: files on an extent device, by name, each committed alone: a SQLite database's
//! (sqlite_extents), or a VM's memory (TASK-456).
//!
//! **Commits.** A file's changes are committed alone ([`FileSet::sync`]): its plan's extents are
//! written, then the root, naming every file's tree as the last commit of each left it, is
//! committed with the device's `commit_root`, a durable barrier. So a crash leaves each file as of
//! a commit of its own, whatever another file had written meanwhile. A commit that fails before
//! its root leaves the file as it was, the extents it wrote freed; one that fails committing the
//! root fails the file set, which may or may not hold it, until it is opened again.
//!
//! **Freeing.** What a commit leaves unreferenced is freed once a root naming it as dying is
//! durable, so that the next opening frees it if a crash came between: [`FREE_BATCH`] of them at a
//! time, a request for many rather than one a commit, the roots naming them meanwhile. A root holds
//! few (the extent daemon's is 4 KiB at most): more, as a commit that compacts, truncates or
//! deletes much leaves, are listed in an extent of their own, written with the commit, which the
//! root names, and which is freed a commit later. The root also says where minting goes on: the
//! extents past it a crash left, written for a commit whose root it lost, are freed by the next
//! opening, as Fxfs's allocator frees its orphans.
//!
//! **The window.** A file set opened with a window ([`FileSet::open_mapped`]) gives each file it
//! opens a region of it, which maps what reads and fetches reach of the file, as its last commit
//! has it (`window`), as they first reach it (TASK-440); and maps again what commits changed, a
//! batch at a time ([`REMAP_BATCH`]), or as pages are fetched: reads of what they changed go to
//! the device meanwhile. Reads of the rest copy from the window, and fetches ([`FileSet::fetch`])
//! read it in place, as SQLite's memory-mapped reads do. While pages fetched are held, nothing is
//! mapped again, as SQLite's own unix VFS remaps nothing while pages are out.
//!
//! **Opening** reads the root alone: a file's tree is read as it is used (`pages`, TASK-440).
//!
//! I/O to the device blocks the calling thread ([`futures::executor::block_on`]), as SQLite's
//! calls do: a file set is not to be called from a thread whose executor the device needs.

use crate::format::{self, Root};
use crate::pages::{Failure, Pages, Plan, REMAP_BATCH};
use crate::window::{Contents, FileSetWindow, Run};
use anyhow::{Context as _, Error, bail};
use futures::executor::block_on;
use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use storage_device::{Device, DeviceHolder, ExtentId, MAX_ROOT};

/// The most sequence numbers an opening frees from either end of what a crash may have left
/// (Fxfs's `MAX_ORPHANS_SOUGHT`).
const MAX_ORPHANS_SOUGHT: u64 = 1 << 16;

/// The most extents freed in one request.
const FREES_AT_ONCE: usize = 4096;

/// The most dying extents a root names itself, and how many wait to be freed at once: 16 bytes
/// each.
const FREE_BATCH: usize = 64;

/// The error of opening a database file the file set doesn't hold.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the file set holds no file {0}")]
pub struct NotFound(pub String);

/// The error of using a file set whose commit failed as it committed its root: whether the device
/// holds that commit, its next opening finds.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the file set failed: {0}")]
pub struct Failed(pub String);

/// How a file set lays out what it writes.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// The most blocks an extent holds: a commit writing more writes several.
    pub max_extent_blocks: u32,
    /// How many regions a window is cut into, a file mapped in each: files opened past them, and
    /// the parts of files past their region's end, are read from the device.
    pub window_regions: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self { max_extent_blocks: 1024, window_regions: 4 }
    }
}

/// A file of a file set.
struct File {
    pages: Pages,
    /// How many connections have it open.
    opens: usize,
    /// Whether a commit made it: one not committed yet goes with its last opening's close.
    committed: bool,
    /// Where its region of the window is, if it has one.
    region: Option<u64>,
    /// How many pages of its region are held, fetched ([`FileSet::fetch`]).
    fetches: u32,
}

impl File {
    fn new(pages: Pages, committed: bool) -> Self {
        Self { pages, opens: 0, committed, region: None, fetches: 0 }
    }
}

/// What a file set holds, under its lock.
struct Inner {
    files: BTreeMap<String, File>,
    /// The sequence number the next extent minted gets.
    mint: u64,
    /// Extents left unreferenced whose frees failed, or have not been made yet.
    dying: Vec<ExtentId>,
    /// The extents listing what the last root holds too much of to hold itself, dying extents or
    /// files, which the next opening reads: dying once a root that names them not is durable, as
    /// the next commit's (TASK-540, TASK-551).
    lists: Vec<ExtentId>,
    failed: Option<String>,
    /// The window's regions no file holds.
    regions: Vec<u64>,
}

/// A file set on a device. See the module docs.
pub struct FileSet {
    device: DeviceHolder,
    options: Options,
    inner: Mutex<Inner>,
    /// The window, if any, and the bytes of each of its regions.
    window: Option<(Box<dyn FileSetWindow>, u64)>,
    /// How many times pages were fetched to read in place.
    fetched: AtomicU64,
}

/// Frees `extents` on `device`, a request at most per [`FREES_AT_ONCE`]: those it failed to free.
async fn free(device: &dyn Device, extents: &[ExtentId]) -> Vec<ExtentId> {
    let mut failed = Vec::new();
    for batch in extents.chunks(FREES_AT_ONCE) {
        for (extent, result) in batch.iter().zip(device.free_extents(batch).await) {
            if let Err(error) = result {
                log::warn!("extent_files: freeing {extent:?}: {error:#}");
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

impl FileSet {
    /// The file set on `device`, made if the device's root is empty.
    pub fn open(device: DeviceHolder, options: Options) -> Result<Arc<Self>, Error> {
        assert!(options.max_extent_blocks > 0, "extents of no blocks");
        let inner = block_on(Self::recover(&**device))?;
        let fetched = AtomicU64::new(0);
        Ok(Arc::new(Self { device, options, inner: Mutex::new(inner), window: None, fetched }))
    }

    /// The file set on `device`, as [`FileSet::open`] opens it, its files mapped into `window`, a
    /// region each, as many regions as `options` says.
    pub fn open_mapped(
        device: DeviceHolder,
        window: Box<dyn FileSetWindow>,
        options: Options,
    ) -> Result<Arc<Self>, Error> {
        assert!(options.max_extent_blocks > 0 && options.window_regions > 0);
        let block_size = device.block_size() as u64;
        let (base, size) = window.memory();
        let region = size / options.window_regions as u64 / block_size * block_size;
        if !(base.as_ptr() as u64).is_multiple_of(block_size) || region == 0 {
            bail!("a window of {size} bytes at {base:?} holds no region of blocks");
        }
        let mut inner = block_on(Self::recover(&**device))?;
        inner.regions = (0..options.window_regions as u64).rev().map(|n| n * region).collect();
        let (window, fetched) = (Some((window, region)), AtomicU64::new(0));
        Ok(Arc::new(Self { device, options, inner: Mutex::new(inner), window, fetched }))
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
            files: BTreeMap::new(),
            mint: next.max(floor),
            dying: Vec::new(),
            lists: Vec::new(),
            failed: None,
            regions: Vec::new(),
        };
        let Some(root) = root else {
            let made = Root { minted: inner.mint, ..Root::default() };
            device.commit_root(&made.encode()).await.context("making the file set")?;
            return Ok(inner);
        };
        inner.mint = inner.mint.max(root.minted);
        let files = match root.files_list {
            Some((list, len)) => {
                let blocks = (len as usize).div_ceil(block_size).max(1);
                let mut buffer = device.allocate_buffer(blocks * block_size).await;
                device.read(list, 0, buffer.as_mut()).await.context("reading the files' list")?;
                format::decode_files(&buffer.as_ref().to_vec())?
            }
            None => root.files,
        };
        for (name, file) in &files {
            let pages = Pages::open(block_size, file).with_context(|| format!("opening {name}"))?;
            inner.files.insert(name.clone(), File::new(pages, true));
        }
        let cell = device.cell();
        let orphans = orphans(root.minted, next).map(|sequence| ExtentId::new(cell, sequence));
        let mut freeing: Vec<_> = root.dying.iter().copied().chain(orphans).collect();
        if let Some((list, count)) = root.dying_list {
            let mut buffer = device.allocate_buffer(format::list_len(count, block_size)).await;
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
        let lists = [root.dying_list, root.files_list].into_iter().flatten();
        inner.lists = lists.map(|(list, _)| list).collect();
        Ok(inner)
    }

    /// Closes the device: every commit is durable already, and the device may keep them
    /// elsewhere too, as an image uploads them to its bucket, which closing it waits for.
    pub fn close(&self) -> Result<(), Error> {
        let mut inner = self.lock()?;
        // What waits to be freed, which the last root names, so that a crash before the device's
        // closing seals the frees frees them again; not the lists it names, which the next opening
        // reads.
        let dying = std::mem::take(&mut inner.dying);
        inner.dying = block_on(free(&**self.device, &dying));
        block_on(self.device.close()).context("closing the device")
    }

    /// The device.
    pub fn device(&self) -> &DeviceHolder {
        &self.device
    }

    fn lock(&self) -> Result<MutexGuard<'_, Inner>, Error> {
        let inner = self.inner.lock().unwrap();
        if let Some(failed) = &inner.failed {
            bail!(Failed(failed.clone()));
        }
        Ok(inner)
    }

    /// The files it holds, committed or open.
    pub fn files(&self) -> Result<Vec<String>, Error> {
        Ok(self.lock()?.files.keys().cloned().collect())
    }

    /// The extents it holds, in order: those its files' last commits reference, every file's
    /// tree read whole for it, and those it has yet to free.
    pub fn extents(&self) -> Result<Vec<ExtentId>, Error> {
        let inner = self.lock()?;
        let mut extents: Vec<_> = inner.dying.iter().chain(&inner.lists).copied().collect();
        for file in inner.files.values() {
            extents.extend(block_on(file.pages.extents(&**self.device))?);
        }
        extents.sort();
        Ok(extents)
    }

    /// How many connections have its files open.
    pub fn opens(&self) -> usize {
        self.inner.lock().unwrap().files.values().map(|file| file.opens).sum()
    }

    /// Opens file `name`, made empty if it isn't there and `create`.
    pub fn open_file(&self, name: &str, create: bool) -> Result<(), Error> {
        let mut inner = self.lock()?;
        let block_size = self.device.block_size() as usize;
        let inner = &mut *inner;
        let file = match inner.files.get_mut(name) {
            Some(file) => file,
            None if create => (inner.files.entry(name.to_owned()))
                .or_insert(File::new(Pages::new(block_size), false)),
            None => bail!(NotFound(name.to_owned())),
        };
        file.opens += 1;
        // A region as it is first opened, if one is free, which maps it as it is read.
        if let Some((window, region_bytes)) = &self.window
            && file.region.is_none()
            && let Some(region) = inner.regions.pop()
        {
            file.region = Some(region);
            let (base, _) = window.memory();
            // SAFETY: within the window, whose memory the region is.
            let at = unsafe { base.add(region as usize) };
            file.pages.set_region(Some((at, *region_bytes)));
        }
        Ok(())
    }

    /// Maps `changed`, ranges of file `name`'s chunks, into its region of the window as its last
    /// commit has them. A failure is logged, and the file reads from the device from then on, its
    /// region given back; but not while pages of it are held, fetched, which read the region in
    /// place: it keeps the region then, what it mapped of `changed` left unmapped, to be mapped
    /// as reads reach it again (TASK-553).
    fn remap(
        &self,
        name: &str,
        file: &mut File,
        regions: &mut Vec<u64>,
        changed: impl IntoIterator<Item = Range<u64>>,
    ) {
        let (Some((window, region_bytes)), Some(region)) = (&self.window, file.region) else {
            return;
        };
        let block_size = self.device.block_size() as u64;
        let mapped = region_bytes / block_size;
        let (mut runs, mut unmapped, mut marked) = (Vec::new(), Vec::new(), Vec::new());
        let device = &**self.device;
        let made = changed.into_iter().try_for_each(|range| {
            let range = range.start.min(mapped)..range.end.min(mapped);
            for (chunks, contents) in block_on(file.pages.runs(device, range))? {
                let (at, len) =
                    (region + chunks.start * block_size, (chunks.end - chunks.start) * block_size);
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
            Ok(()) => file.pages.mark_mapped(marked),
            Err(error) if file.fetches > 0 => {
                log::error!("extent_files: mapping {name}, its pages held: {error:#}");
            }
            Err(error) => {
                log::error!(
                    "extent_files: mapping {name}, read from the device from now on: {error:#}"
                );
                self.release_region(name, file, regions);
            }
        }
    }

    fn release_region(&self, name: &str, file: &mut File, regions: &mut Vec<u64>) {
        if let Some(region) = file.region.take() {
            let (window, bytes) = self.window.as_ref().expect("a region belongs to a window");
            file.pages.set_region(None);
            match window.unmap(region, *bytes) {
                Ok(()) => regions.push(region),
                Err(error) => log::error!("extent_files: unmapping {name}'s region: {error:#}"),
            }
        }
    }

    fn remove_file(&self, inner: &mut Inner, name: &str) -> File {
        let mut file = inner.files.remove(name).expect("a file to remove");
        self.release_region(name, &mut file, &mut inner.regions);
        file
    }

    /// Maps file `name`'s chunks commits changed since its region was last mapped, as the last
    /// has them: unless pages of it are held, fetched, which read as they were until given back,
    /// as SQLite's own unix VFS remaps nothing while pages are out.
    fn refresh(&self, name: &str, file: &mut File, regions: &mut Vec<u64>) {
        if file.region.is_some() && file.fetches == 0 && file.pages.stale_chunks() > 0 {
            let stale = file.pages.take_stale();
            self.remap(name, file, regions, stale);
        }
    }

    /// Maps what the region of file `name` doesn't yet of the chunks `len` bytes at `offset`
    /// reach, as its last commit has them.
    fn map_reached(
        &self,
        name: &str,
        file: &mut File,
        regions: &mut Vec<u64>,
        offset: u64,
        len: u64,
    ) {
        if file.region.is_none() || len == 0 {
            return;
        }
        let block_size = self.device.block_size() as u64;
        let unmapped =
            file.pages.unmapped(offset / block_size..(offset + len).div_ceil(block_size));
        if !unmapped.is_empty() {
            self.remap(name, file, regions, unmapped);
        }
    }

    /// Where the `len` bytes of file `name` at `offset` are in the window, if it maps them as
    /// they read: to read in place until given back ([`FileSet::unfetch`]).
    pub fn fetch(&self, name: &str, offset: u64, len: u64) -> Result<Option<*const u8>, Error> {
        let mut inner = self.lock()?;
        let inner = &mut *inner;
        let file = inner.files.get_mut(name).expect("a file open");
        if file.region.is_none() {
            return Ok(None);
        }
        self.refresh(name, file, &mut inner.regions);
        self.map_reached(name, file, &mut inner.regions, offset, len);
        let fetched = block_on(file.pages.fetch(&**self.device, offset, len))?;
        file.fetches += fetched.is_some() as u32;
        self.fetched.fetch_add(fetched.is_some() as u64, Ordering::Relaxed);
        Ok(fetched)
    }

    /// Whether its files are mapped into a window ([`FileSet::open_mapped`]), which reads in place
    /// ([`FileSet::fetch`]) need.
    pub fn is_mapped(&self) -> bool {
        self.window.is_some()
    }

    /// How many times pages were fetched to read in place ([`FileSet::fetch`]).
    pub fn fetched(&self) -> u64 {
        self.fetched.load(Ordering::Relaxed)
    }

    /// Gives back a page of file `name` fetched ([`FileSet::fetch`]).
    pub fn unfetch(&self, name: &str) {
        let mut inner = self.inner.lock().unwrap();
        let file = inner.files.get_mut(name).expect("a file open");
        file.fetches = file.fetches.checked_sub(1).expect("a page fetched");
    }

    /// Closes file `name`, opened once more than closed.
    pub fn close_file(&self, name: &str) {
        let mut inner = self.inner.lock().unwrap();
        let file = inner.files.get_mut(name).expect("a file open");
        file.opens -= 1;
        if file.pages.changed() {
            log::warn!("extent_files: {name} closed with changes not committed, which are lost");
        }
        if file.opens == 0 && !file.committed {
            self.remove_file(&mut inner, name);
        }
    }

    fn file<'a>(inner: &'a mut Inner, name: &str) -> &'a mut File {
        inner.files.get_mut(name).expect("a file open")
    }

    /// Reads file `name` into `buf` from `offset`, as far as it goes: how many bytes it read.
    pub fn read(&self, name: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        let mut inner = self.lock()?;
        let inner = &mut *inner;
        let file = inner.files.get_mut(name).expect("a file open");
        self.map_reached(name, file, &mut inner.regions, offset, buf.len() as u64);
        block_on(file.pages.read(&**self.device, offset, buf))
    }

    pub fn write(&self, name: &str, offset: u64, data: &[u8]) -> Result<(), Error> {
        let mut inner = self.lock()?;
        let file = Self::file(&mut inner, name);
        block_on(file.pages.write(&**self.device, offset, data))
    }

    /// The bytes of a file one node of its tree names: what [`FileSet::runs`] of them reads one
    /// node for, and the blocks around it maps, once the nodes above it are read.
    pub fn node_bytes(&self) -> u64 {
        let block_size = self.device.block_size() as usize;
        (format::fanout(block_size) * block_size) as u64
    }

    /// What file `name` holds of the `len` bytes at `offset`, whole blocks, as of its last commit:
    /// runs of the file's bytes, each blocks of one extent from an offset in it, zeros (holes), or
    /// nothing (past the file's end), the file's tree read as far as they need. For what maps the
    /// file into memory of its own, as a VM's memory window does (TASK-456.3), not through the
    /// file set's window.
    pub fn runs(
        &self,
        name: &str,
        offset: u64,
        len: u64,
    ) -> Result<Vec<(Range<u64>, Option<Contents>)>, Error> {
        let block_size = self.device.block_size() as u64;
        if !offset.is_multiple_of(block_size) || !len.is_multiple_of(block_size) {
            bail!("the runs of {len} bytes at {offset}, not whole blocks of {block_size}");
        }
        let mut inner = self.lock()?;
        let file = Self::file(&mut inner, name);
        let chunks = offset / block_size..(offset + len) / block_size;
        let runs = block_on(file.pages.runs(&**self.device, chunks))?;
        let bytes = |chunks: Range<u64>| chunks.start * block_size..chunks.end * block_size;
        Ok(runs.into_iter().map(|(chunks, contents)| (bytes(chunks), contents)).collect())
    }

    /// Makes the `len` bytes of file `name` at `offset`, whole blocks, holes: they read as zeros,
    /// and its next commit stores nothing for them (TASK-456.2).
    pub fn punch(&self, name: &str, offset: u64, len: u64) -> Result<(), Error> {
        let block_size = self.device.block_size() as u64;
        if !offset.is_multiple_of(block_size) || !len.is_multiple_of(block_size) {
            bail!("punching {len} bytes at {offset}, not whole blocks of {block_size}");
        }
        let mut inner = self.lock()?;
        let file = Self::file(&mut inner, name);
        let chunks = offset / block_size..(offset + len) / block_size;
        block_on(file.pages.punch(&**self.device, chunks))
    }

    pub fn truncate(&self, name: &str, size: u64) -> Result<(), Error> {
        let mut inner = self.lock()?;
        let file = Self::file(&mut inner, name);
        block_on(file.pages.truncate(&**self.device, size))
    }

    pub fn size(&self, name: &str) -> Result<u64, Error> {
        let mut inner = self.lock()?;
        Ok(Self::file(&mut inner, name).pages.size())
    }

    /// Commits what changed in file `name` since its last commit, durably.
    pub fn sync(&self, name: &str) -> Result<(), Error> {
        let mut inner = self.lock()?;
        self.commit(&mut inner, name)
    }

    /// Whether file `name` exists.
    pub fn exists(&self, name: &str) -> Result<bool, Error> {
        Ok(self.lock()?.files.contains_key(name))
    }

    /// Deletes file `name`, durably: whether it was there.
    pub fn delete(&self, name: &str) -> Result<bool, Error> {
        let mut inner = self.lock()?;
        let Some(file) = inner.files.get(name) else { return Ok(false) };
        if file.opens > 0 {
            bail!("deleting {name}, open");
        }
        if file.committed {
            let dead: Vec<_> = block_on(file.pages.extents(&**self.device))?.into_iter().collect();
            self.commit_root(&mut inner, name, dead, None)?;
        }
        self.remove_file(&mut inner, name);
        Ok(true)
    }

    /// Commits file `name`'s changes, if any, or makes it if no commit has.
    fn commit(&self, inner: &mut Inner, name: &str) -> Result<(), Error> {
        let device = &**self.device;
        let (cell, mut next) = (device.cell(), inner.mint);
        let file = Self::file(inner, name);
        if !file.pages.changed() && file.committed {
            return Ok(());
        }
        let mint = || {
            next += 1;
            ExtentId::new(cell, next - 1)
        };
        let plan = block_on(file.pages.plan(device, mint, self.options.max_extent_blocks));
        // What it minted is spent, whatever came of it.
        inner.mint = next;
        let plan = plan?;
        let committed = self.commit_root(inner, name, plan.dead.clone(), Some(&plan));
        committed.with_context(|| format!("committing {name}"))?;
        let inner = &mut *inner;
        let file = inner.files.get_mut(name).expect("a file open");
        file.pages.apply(plan);
        file.committed = true;
        // Mapped again a batch at a time: read from memory or the device meanwhile.
        if file.pages.stale_chunks() >= REMAP_BATCH {
            self.refresh(name, file, &mut inner.regions);
        }
        Ok(())
    }

    /// Commits the root: each file committed as it is, except `name`, replaced by `plan` or deleted. The plan's
    /// extents written first, the last write carrying the root; and `dead` among the dying, freed
    /// once a batch of them waits ([`FREE_BATCH`]), their root durable. A failure before the root
    /// changes nothing, but that what it wrote is freed; one committing it fails the file set.
    fn commit_root(
        &self,
        inner: &mut Inner,
        name: &str,
        dead: Vec<ExtentId>,
        plan: Option<&Plan>,
    ) -> Result<(), Error> {
        let device = &**self.device;
        let mut files: BTreeMap<_, _> = (inner.files.iter())
            .filter(|(_, file)| file.committed)
            .map(|(name, file)| (name.clone(), file.pages.root()))
            .collect();
        if let Some(plan) = plan {
            files.insert(name.to_owned(), plan.root.clone());
        } else {
            files.remove(name);
        }
        // The last root's lists among them: the root this commits names them not.
        let carried = std::mem::take(&mut inner.dying);
        let named = std::mem::take(&mut inner.lists);
        let dying: Vec<_> = carried.iter().chain(&named).copied().chain(dead).collect();
        let block_size = device.block_size() as usize;
        let mut mint = || {
            inner.mint += 1;
            ExtentId::new(device.cell(), inner.mint - 1)
        };
        // What the root holds too much of, listed in extents of its own, written with it: too many
        // dying extents, and files too many or too long for a device's root.
        let mut lists: Vec<(ExtentId, Vec<u8>)> = Vec::new();
        let mut root = Root { dying: dying.clone(), files, ..Root::default() };
        if dying.len() > FREE_BATCH {
            let list = mint();
            (root.dying, root.dying_list) = (Vec::new(), Some((list, dying.len() as u64)));
            lists.push((list, format::encode_list(&dying, block_size)));
        }
        if root.encode().len() > MAX_ROOT {
            let list = mint();
            let mut bytes = format::encode_files(&std::mem::take(&mut root.files));
            root.files_list = Some((list, bytes.len() as u64));
            bytes.resize(bytes.len().div_ceil(block_size) * block_size, 0);
            lists.push((list, bytes));
        }
        root.minted = inner.mint;
        let encoded = root.encode();
        assert!(encoded.len() <= MAX_ROOT, "a root of {} bytes", encoded.len());
        let written = block_on(async {
            for (list, bytes) in &lists {
                let mut buffer = device.allocate_buffer(bytes.len()).await;
                buffer.as_mut().copy_from_slice(bytes);
                let listed = device.write(*list, 0, buffer.as_ref()).await;
                listed.context("writing the root's lists").map_err(Failure::Writing)?;
            }
            match plan {
                Some(plan) => plan.write(device, Some(&encoded)).await,
                None => device.commit_root(&encoded).await.map_err(Failure::Committing),
            }
        });
        match written {
            Ok(()) => {}
            Err(Failure::Writing(error)) => {
                // Nothing names what it wrote: freed, or, failing that, dying.
                let wrote: Vec<_> = (lists.iter().map(|(list, _)| *list))
                    .chain(plan.into_iter().flat_map(Plan::extents))
                    .collect();
                (inner.dying, inner.lists) = (carried, named);
                inner.dying.extend(block_on(free(device, &wrote)));
                return Err(error);
            }
            Err(Failure::Committing(error)) => {
                let error = error.context("committing the file set's root");
                inner.failed = Some(format!("{error:#}"));
                return Err(error);
            }
        }
        inner.dying = match root.dying_list.is_some() || dying.len() >= FREE_BATCH {
            true => block_on(free(device, &dying)),
            false => dying,
        };
        inner.lists = lists.into_iter().map(|(list, _)| list).collect();
        Ok(())
    }

    /// Fails unless the file set is usable: what its users see as an error of their own.
    pub fn check(&self) -> Result<(), Error> {
        self.lock().map(drop)
    }
}

impl std::fmt::Debug for FileSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.lock().map_err(|_| std::fmt::Error)?;
        f.debug_struct("FileSet")
            .field("files", &inner.files.keys().collect::<Vec<_>>())
            .field("mint", &inner.mint)
            .field("failed", &inner.failed)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use storage_device::fake_device::{FakeDevice, Op};

    const BLOCK: usize = 512;

    /// The runs of a file as of its last commit: its blocks' extents, zeros where it was punched,
    /// nothing past its end; what was written since is not among them (TASK-456.3).
    #[test]
    fn runs_say_what_the_last_commit_holds() {
        let device = FakeDevice::new(1 << 20, BLOCK as u32).calm();
        let files = FileSet::open(DeviceHolder::new(device), Options::default()).unwrap();
        files.open_file("memory", true).unwrap();
        files.write("memory", 0, &vec![1; BLOCK * 8]).unwrap();
        files.punch("memory", 2 * BLOCK as u64, 2 * BLOCK as u64).unwrap();
        files.sync("memory").unwrap();
        files.write("memory", 0, &vec![2; BLOCK]).unwrap();
        let block = BLOCK as u64;
        let runs = files.runs("memory", 0, 10 * block).unwrap();
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
        // The committed bytes, not those written since.
        let Some(Contents::Extent { extent, offset }) = runs[0].1 else { panic!("an extent") };
        let device = files.device();
        let mut buffer = block_on(device.allocate_buffer(BLOCK));
        block_on(device.read(extent, offset, buffer.as_mut())).unwrap();
        let mut bytes = vec![0; BLOCK];
        buffer.as_ref().copy_to_slice(&mut bytes);
        assert_eq!(bytes, vec![1; BLOCK]);
        let unaligned = files.runs("memory", 1, block).unwrap_err().to_string();
        assert_eq!(unaligned, "the runs of 512 bytes at 1, not whole blocks of 512");
    }

    /// A file set's opening reads its root and none of its files' trees; a read of a page reads
    /// the nodes on its path and the page, and the file holds those nodes alone (TASK-440).
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
        let file_set = FileSet::open(DeviceHolder::new(device), Options::default()).unwrap();
        file_set.open_file("a.db", true).unwrap();
        file_set.write("a.db", 0, &vec![3; BLOCK * 401]).unwrap();
        file_set.sync("a.db").unwrap();
        file_set.close_file("a.db");

        let before = reads.load(Ordering::Relaxed);
        let mut inner = block_on(FileSet::recover(&***file_set.device())).unwrap();
        // The root, which the fake device counts as a read, alone.
        assert_eq!(reads.load(Ordering::Relaxed) - before, 1, "reads at the opening");
        let file = inner.files.get_mut("a.db").unwrap();
        assert_eq!((file.pages.root().height, file.pages.nodes_held()), (3, 0));
        let mut page = vec![0; BLOCK];
        block_on(file.pages.read(&***file_set.device(), BLOCK as u64 * 250, &mut page)).unwrap();
        assert_eq!(page, vec![3; BLOCK]);
        assert_eq!(reads.load(Ordering::Relaxed) - before, 1 + 4, "reads of one page");
        assert_eq!(file.pages.nodes_held(), 3);
    }
}
