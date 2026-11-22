// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A block device's contents: its bytes, in chunks of a block each, kept in write-once extents.
//!
//! **The tree.** Chunk `k` is a block of some extent, or a hole, which reads as zeros. The device
//! keeps where each chunk is in a radix tree of nodes (`format`): node `j` of level 1 names chunks
//! `j * fanout..`, node `j` of level `l` names nodes `j * fanout..` of level `l - 1`, and the top
//! node, the one node of the highest level, is what the root names. A node whose children are all
//! holes is a hole too.
//!
//! **Reading it lazily** (TASK-440). Opening the device reads nothing: it knows its root alone. A
//! node is read as a lookup first passes through it, from the top down, and kept: so what the
//! device holds in memory grows with what was read and written of it, not with its size. A commit
//! writes the nodes it changes from what it read of them.
//!
//! **Changes.** Writes go to chunks in memory, read in first unless written whole, until a commit
//! ([`Pages::plan`], [`Plan::write`], [`Pages::apply`]) writes them to new extents, with the nodes
//! above them, which name them, and the nodes above those, to the top: copy on write. A resize to
//! fewer bytes cuts the chunks past it, which read as holes from then on. A discard makes whole
//! chunks holes, which a commit stores as nothing: a VM's memory keeps its zero pages so
//! (TASK-456.2).
//!
//! **Space.** Each extent ends with its blocks' owners (`format`): which chunk or node each holds.
//! A block a commit replaces is dead. The commit reads the owners of the extents it leaves blocks
//! of, and looks each up in the tree: an extent none of whose blocks the tree names any more is
//! freed; one less than half live is named in the root, and the next commit writes its live blocks
//! again, with its own changes, so that it dies.

use crate::format::{self, Owner, Ref, SPARSE_KEPT, TreeRoot};
use crate::window::{Contents, Extents};
use anyhow::{Context as _, Error, ensure};
use futures::StreamExt as _;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;
use storage_device::{Device, ExtentId, WriteFlags, WriteOptions};

/// The reads a lookup of many nodes keeps in flight.
const READS_IN_FLIGHT: usize = 32;

/// The most bytes one data transfer to the device carries.
const TRANSFER_PIECE: usize = 1 << 20;

/// How many chunks commits change before the window maps them again, one request for them all
/// rather than one a commit, each chunk once however often changed; and the most of what they
/// wrote the device keeps meanwhile, to read from memory: 4 MiB at most (TASK-552).
pub(crate) const REMAP_BATCH: u64 = 1024;

/// A node's children: a ref, or a hole, for each of `fanout`, holes past the tree's end.
type Node = Box<[Option<Ref>]>;

/// A tree node's place: its level, and its index at that level.
type Place = (u32, u64);

/// The chunks of contents of `size` bytes, in blocks of `block_size`.
fn chunks(size: u64, block_size: usize) -> u64 {
    size.div_ceil(block_size as u64)
}

/// How many of each level a tree of `chunks` holds: chunks first, the top's one last. A tree of
/// no chunks has no nodes; one of any has one level of nodes at least.
fn level_counts(chunks: u64, fanout: u64) -> Vec<u64> {
    let mut counts = vec![chunks];
    while chunks > 0 && (counts.len() == 1 || *counts.last().unwrap() > 1) {
        counts.push(counts.last().unwrap().div_ceil(fanout));
    }
    counts
}

/// Reads `len` bytes of `extent` at `offset`.
async fn read_bytes(
    device: &dyn Device,
    extent: ExtentId,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>, Error> {
    let mut buffer = device.allocate_buffer(len).await;
    device
        .read(extent, offset, buffer.as_mut())
        .await
        .with_context(|| format!("reading {len} bytes of {extent:?} at {offset}"))?;
    Ok(buffer.as_ref().to_vec())
}

/// Reads block `block` of `extent` into `out`, a block long.
async fn read_block(
    device: &dyn Device,
    extent: ExtentId,
    block: u32,
    out: &mut [u8],
) -> Result<(), Error> {
    let read = read_bytes(device, extent, block as u64 * out.len() as u64, out.len()).await?;
    out.copy_from_slice(&read);
    Ok(())
}

/// The owners of `blocks` blocks of `extent`, which follow them (`format`).
async fn read_owners(
    device: &dyn Device,
    extent: ExtentId,
    blocks: u32,
    block_size: usize,
) -> Result<Vec<Owner>, Error> {
    let len = format::owner_blocks(blocks, block_size) as usize * block_size;
    let bytes = read_bytes(device, extent, blocks as u64 * block_size as u64, len).await?;
    format::decode_owners(&bytes, blocks).with_context(|| format!("the owners of {extent:?}"))
}

/// The owners of each of `extents`, which hold the blocks given, read at once.
async fn read_all_owners(
    device: &dyn Device,
    extents: impl IntoIterator<Item = (ExtentId, u32)>,
    block_size: usize,
) -> Result<HashMap<ExtentId, Vec<Owner>>, Error> {
    let mut reads = futures::stream::iter(extents)
        .map(|(extent, blocks)| async move {
            Ok::<_, Error>((extent, read_owners(device, extent, blocks, block_size).await?))
        })
        .buffered(READS_IN_FLIGHT);
    let mut owners = HashMap::new();
    while let Some(read) = reads.next().await {
        let (extent, read) = read?;
        owners.insert(extent, read);
    }
    Ok(owners)
}

/// A committed tree's shape and the nodes of it read: what lookups in it need. See the module
/// docs.
struct Tree {
    block_size: usize,
    fanout: u64,
    /// The root's top node, if any.
    top: Option<Ref>,
    /// The counts of its levels ([`level_counts`]).
    counts: Vec<u64>,
    /// The nodes read, or written by a commit: their children.
    nodes: HashMap<Place, Node>,
}

impl Tree {
    fn height(&self) -> u32 {
        self.counts.len() as u32 - 1
    }

    /// Whether the tree has `place`, of any level, chunks being level 0.
    fn has(&self, (level, index): Place) -> bool {
        level <= self.height() && index < self.counts[level as usize]
    }

    /// The node above `place`.
    fn parent(&self, (level, index): Place) -> Place {
        (level + 1, index / self.fanout)
    }

    /// The ref at `place`, a chunk or a node, its node above it read: none for a hole, or outside
    /// the tree.
    fn at(&self, place: Place) -> Option<Ref> {
        if !self.has(place) {
            return None;
        }
        if place.0 == self.height() {
            return self.top;
        }
        let node = self.nodes.get(&self.parent(place)).expect("the node above a lookup read");
        node[(place.1 % self.fanout) as usize]
    }

    /// Reads the nodes of `places`, and those above them, that it hasn't read: a level at a time,
    /// top down, reads in flight at once. Places outside the tree are left out.
    async fn load(
        &mut self,
        device: &dyn Device,
        places: impl IntoIterator<Item = Place>,
    ) -> Result<(), Error> {
        let mut wanted: BTreeSet<Place> = BTreeSet::new();
        for place in places {
            let mut place = place;
            while place.0 >= 1 && self.has(place) && !self.nodes.contains_key(&place) {
                if !wanted.insert(place) {
                    break;
                }
                place = self.parent(place);
            }
        }
        for level in (1..=self.height()).rev() {
            let reads: Vec<(u64, Option<Ref>)> = (wanted.range((level, 0)..=(level, u64::MAX)))
                .map(|&(_, index)| (index, self.at((level, index))))
                .collect();
            let (block_size, fanout) = (self.block_size, self.fanout);
            let children = self.counts[level as usize - 1];
            let mut read = futures::stream::iter(reads)
                .map(|(index, at)| async move {
                    let mut node: Node = vec![None; fanout as usize].into();
                    if let Some(at) = at {
                        let mut block = vec![0; block_size];
                        read_block(device, at.extent, at.block, &mut block).await?;
                        let count = (children - index * fanout).min(fanout) as usize;
                        let refs = format::decode_node(&block, level, index, count)?;
                        node[..count].copy_from_slice(&refs);
                    }
                    Ok::<_, Error>((index, node))
                })
                .buffered(READS_IN_FLIGHT);
            while let Some(read) = read.next().await {
                let (index, node) = read?;
                self.nodes.insert((level, index), node);
            }
        }
        Ok(())
    }

    /// Visits allocated places on paths to `chunks`, top down, each level in order: reading the
    /// nodes holding any, and no node of holes.
    async fn visit(
        &mut self,
        device: &dyn Device,
        chunks: Range<u64>,
        mut visit: impl FnMut(Place, Ref),
    ) -> Result<(), Error> {
        let Some(top) = self.top else { return Ok(()) };
        let chunks = chunks.start..chunks.end.min(self.counts[0]);
        if chunks.is_empty() {
            return Ok(());
        }
        // The first and last places of each level on paths to them.
        let mut boundary = vec![(chunks.start, chunks.end - 1)];
        for level in 1..=self.height() {
            let (first, last) = boundary[level as usize - 1];
            boundary.push((first / self.fanout, last / self.fanout));
        }
        visit((self.height(), 0), top);
        let mut nodes = vec![0];
        for level in (1..=self.height()).rev() {
            self.load(device, nodes.iter().map(|&index| (level, index))).await?;
            let mut below = Vec::new();
            for index in nodes {
                for (slot, child) in self.nodes[&(level, index)].iter().enumerate() {
                    let child_index = index * self.fanout + slot as u64;
                    let (first, last) = boundary[level as usize - 1];
                    if let Some(child) = child
                        && (first..=last).contains(&child_index)
                    {
                        visit((level - 1, child_index), *child);
                        if level > 1 {
                            below.push(child_index);
                        }
                    }
                }
            }
            nodes = below;
        }
        Ok(())
    }

    /// The refs at `places`, read as needed.
    async fn lookup(
        &mut self,
        device: &dyn Device,
        places: &[Place],
    ) -> Result<Vec<Option<Ref>>, Error> {
        let parents: Vec<Place> = places.iter().map(|place| self.parent(*place)).collect();
        self.load(device, parents).await?;
        Ok(places.iter().map(|place| self.at(*place)).collect())
    }
}

/// A block device's contents. See the module docs.
pub(crate) struct Pages {
    /// The contents as of the last commit, but the tree.
    committed: TreeRoot,
    tree: Tree,
    /// The size as written since.
    size: u64,
    /// The chunks from which those of the last commit read as holes: cut since.
    cut: u64,
    /// The chunks written since, whole.
    dirty: BTreeMap<u64, Box<[u8]>>,
    /// The chunks discarded since that the last commit holds data in, none of them in `dirty`:
    /// ranges, sorted, apart, as a VM's memory discards its zero pages by the million
    /// (TASK-584). What the last commit holds as holes already is left out: discarded, it changes
    /// nothing.
    holes: Vec<Range<u64>>,
    /// The window's region, if the device has one (`window`): its first byte's address, its length,
    /// and which of its chunks it maps, as the last commit has them or as before commits since
    /// ([`Pages::mark_stale`]).
    region: Option<Region>,
    /// The ranges of chunks commits changed since the region mapped them, which it maps as
    /// before them, and how many chunks they hold.
    stale: Vec<Range<u64>>,
    stale_chunks: u64,
    /// What those commits wrote of the chunks the region maps as before them, read from here
    /// until it maps them again: as a transaction reads again what the one before wrote, the
    /// header of the database's first page among them. [`REMAP_BATCH`] of them at most: past
    /// that, reads of the rest go to the device (TASK-552).
    recent: BTreeMap<u64, Box<[u8]>>,
}

/// The region of the window the contents are mapped into.
struct Region {
    base: usize,
    len: u64,
    /// One bit for each of its chunks: whether it maps it.
    mapped: Vec<u64>,
}

impl Region {
    fn chunks(&self, block_size: usize) -> u64 {
        self.len / block_size as u64
    }

    fn is_mapped(&self, index: u64) -> bool {
        (self.mapped.get((index / 64) as usize)).is_some_and(|word| word & (1 << (index % 64)) != 0)
    }

    fn set_mapped(&mut self, index: u64, mapped: bool) {
        let (word, bit) = ((index / 64) as usize, 1 << (index % 64));
        match mapped {
            true => self.mapped[word] |= bit,
            false => self.mapped[word] &= !bit,
        }
    }
}

/// `ranges`, sorted and merged where they meet.
fn merged(mut ranges: Vec<Range<u64>>) -> Vec<Range<u64>> {
    ranges.retain(|range| !range.is_empty());
    ranges.sort_by_key(|range| range.start);
    let mut out: Vec<Range<u64>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match out.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => out.push(range),
        }
    }
    out
}

impl Pages {
    /// Contents of no bytes, never committed.
    pub fn new(block_size: usize) -> Self {
        Self::open(block_size, &TreeRoot::default()).expect("an empty tree's root")
    }

    /// The contents `root` names, on a device of blocks of `block_size`: nothing read yet.
    pub fn open(block_size: usize, root: &TreeRoot) -> Result<Self, Error> {
        let fanout = format::fanout(block_size) as u64;
        let counts = level_counts(chunks(root.size, block_size), fanout);
        let height = counts.len() as u32 - 1;
        ensure!(
            root.height == height,
            "contents of {} bytes, of height {}",
            root.size,
            root.height
        );
        ensure!(root.top.is_none() || height > 0, "contents of no chunks with a top node");
        let cut = counts[0];
        let tree = Tree { block_size, fanout, top: root.top, counts, nodes: HashMap::new() };
        Ok(Self {
            committed: root.clone(),
            tree,
            size: root.size,
            cut,
            dirty: BTreeMap::new(),
            holes: Vec::new(),
            region: None,
            stale: Vec::new(),
            stale_chunks: 0,
            recent: BTreeMap::new(),
        })
    }

    fn block_size(&self) -> usize {
        self.tree.block_size
    }

    /// The size, as written.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Whether anything changed since the last commit.
    pub fn changed(&self) -> bool {
        !self.dirty.is_empty()
            || !self.holes.is_empty()
            || self.size != self.committed.size
            || self.cut < self.tree.counts[0]
    }

    /// Whether chunk `index` was discarded since the last commit, which held data there.
    fn discarded(&self, index: u64) -> bool {
        let at = self.holes.partition_point(|range| range.end <= index);
        self.holes.get(at).is_some_and(|range| range.start <= index)
    }

    /// Takes chunk `index` out of what was discarded since, to be written.
    fn undiscard(&mut self, index: u64) {
        let at = self.holes.partition_point(|range| range.end <= index);
        let Some(range) = self.holes.get(at).cloned() else { return };
        if range.start > index {
            return;
        }
        let parts = [range.start..index, index + 1..range.end];
        self.holes.splice(at..at + 1, parts.into_iter().filter(|part| !part.is_empty()));
    }

    /// Whether the region maps chunk `index` as before commits that changed it.
    fn is_stale(&self, index: u64) -> bool {
        let at = self.stale.partition_point(|range| range.end <= index);
        self.stale.get(at).is_some_and(|range| range.start <= index)
    }

    /// The contents as the last commit left them.
    #[cfg(test)]
    pub fn root(&self) -> TreeRoot {
        self.committed.clone()
    }

    /// How many of its tree's nodes it holds in memory: those read or written.
    #[cfg(test)]
    pub fn nodes_held(&self) -> usize {
        self.tree.nodes.len()
    }

    /// The extents the contents hold as of the last commit: every one its tree names, read whole,
    /// none of it kept.
    pub async fn extents(&self, device: &dyn Device) -> Result<BTreeSet<ExtentId>, Error> {
        let mut tree = Tree {
            block_size: self.tree.block_size,
            fanout: self.tree.fanout,
            top: self.tree.top,
            counts: self.tree.counts.clone(),
            nodes: HashMap::new(),
        };
        let mut extents = BTreeSet::new();
        tree.visit(device, 0..u64::MAX, |_, at| {
            extents.insert(at.extent);
        })
        .await?;
        Ok(extents)
    }

    /// Where chunk `index` is as of the last commit, as the contents read now, its node read: a
    /// hole once cut.
    fn committed_chunk(&self, index: u64) -> Option<Ref> {
        (index < self.cut).then(|| self.tree.at((0, index))).flatten()
    }

    /// Reads the nodes naming `chunks`, those of the last commit's the contents still have.
    async fn load_chunks(&mut self, device: &dyn Device, chunks: Range<u64>) -> Result<(), Error> {
        let chunks = chunks.start..chunks.end.min(self.cut);
        let fanout = self.tree.fanout;
        let nodes = match chunks.is_empty() {
            true => 0..0,
            false => chunks.start / fanout..(chunks.end - 1) / fanout + 1,
        };
        self.tree.load(device, nodes.map(|index| (1, index))).await
    }

    /// Reads chunk `index` as of the last commit, as the contents read now, its node read, into
    /// `out`: from the window's memory if it maps it.
    async fn read_committed(
        &self,
        device: &dyn Device,
        index: u64,
        out: &mut [u8],
    ) -> Result<(), Error> {
        if let (Some(chunk), true) = (self.recent.get(&index), index < self.cut) {
            out.copy_from_slice(chunk);
            return Ok(());
        }
        match self.committed_chunk(index) {
            None => out.fill(0),
            Some(at) => match self.mapped(index * self.block_size() as u64, out.len() as u64) {
                Some(at) => {
                    // SAFETY: the window maps the chunk, read-only, until the next commit
                    // marks it stale, which takes `&mut self` (mark_stale), not while this read
                    // borrows it.
                    out.copy_from_slice(unsafe { std::slice::from_raw_parts(at, out.len()) })
                }
                None => read_block(device, at.extent, at.block, out).await?,
            },
        }
        Ok(())
    }

    /// Gives the contents the window's region `region`, its memory's first byte and length, mapping
    /// nothing yet; or none, its region taken away.
    pub fn set_region(&mut self, region: Option<(std::ptr::NonNull<u8>, u64)>) {
        let block_size = self.block_size() as u64;
        self.region = region.map(|(base, len)| Region {
            base: base.as_ptr() as usize,
            len,
            mapped: vec![0; (len / block_size).div_ceil(64) as usize],
        });
        self.stale.clear();
        self.stale_chunks = 0;
        self.recent.clear();
    }

    /// Notes what the region maps of `chunks` now, as the last commit has them: the runs of it
    /// mapped, `true`, or unmapped, past the end.
    pub fn mark_mapped(&mut self, runs: impl IntoIterator<Item = (Range<u64>, bool)>) {
        let Some(region) = &mut self.region else { return };
        for (chunks, mapped) in runs {
            for index in chunks {
                region.set_mapped(index, mapped);
            }
        }
    }

    /// The ranges of `chunks` within the region that it doesn't map: to map before a read, unless
    /// past the end as of its last commit.
    pub fn unmapped(&self, chunks: Range<u64>) -> Vec<Range<u64>> {
        let Some(region) = &self.region else { return Vec::new() };
        let end = chunks.end.min(region.chunks(self.block_size())).min(self.tree.counts[0]);
        let mut out: Vec<Range<u64>> = Vec::new();
        for index in chunks.start..end.max(chunks.start) {
            if region.is_mapped(index) {
                continue;
            }
            match out.last_mut() {
                Some(range) if range.end == index => range.end += 1,
                _ => out.push(index..index + 1),
            }
        }
        out
    }

    /// Where the `len` bytes at `offset` are in the window's memory, if it maps them as the last
    /// commit has them.
    fn mapped(&self, offset: u64, len: u64) -> Option<*const u8> {
        let region = self.region.as_ref()?;
        let block_size = self.block_size() as u64;
        let chunks = offset / block_size..(offset + len).div_ceil(block_size);
        let stale =
            self.stale.iter().any(|range| range.start < chunks.end && chunks.start < range.end);
        let whole =
            offset + len <= region.len && chunks.clone().all(|index| region.is_mapped(index));
        (whole && !stale).then(|| (region.base + offset as usize) as *const u8)
    }

    /// The region maps those of `ranges` of chunks it mapped as before the last commit, which
    /// changed them, until mapped again ([`Pages::take_stale`]): reads of them read what the
    /// commits wrote, or go to the device, meanwhile. Each chunk is counted once, however many
    /// commits changed it, as a database's first pages are changed by each.
    fn mark_stale(&mut self, ranges: impl IntoIterator<Item = Range<u64>>) {
        let Some(region) = &self.region else { return };
        let mut stale = std::mem::take(&mut self.stale);
        for range in ranges {
            let end = range.end.min(region.chunks(self.tree.block_size));
            for index in range.start..end.max(range.start) {
                if !region.is_mapped(index) {
                    continue;
                }
                match stale.last_mut() {
                    Some(last) if last.end == index => last.end += 1,
                    _ => stale.push(index..index + 1),
                }
            }
        }
        self.stale = merged(stale);
        self.stale_chunks = self.stale.iter().map(|range| range.end - range.start).sum();
    }

    /// How many chunks the window's region maps as before commits that changed them.
    pub fn stale_chunks(&self) -> u64 {
        self.stale_chunks
    }

    /// The ranges of chunks to map again, which the caller maps as the last commit has them.
    pub fn take_stale(&mut self) -> Vec<Range<u64>> {
        self.stale_chunks = 0;
        self.recent.clear();
        std::mem::take(&mut self.stale)
    }

    /// Where the `len` bytes at `offset` are in the window's memory, if it maps them and they
    /// read as the last commit left them: unchanged since, and no hole.
    pub async fn fetch(
        &mut self,
        device: &dyn Device,
        offset: u64,
        len: u64,
    ) -> Result<Option<*const u8>, Error> {
        let block_size = self.block_size() as u64;
        if len == 0 || offset + len > self.committed.size.min(self.size) {
            return Ok(None);
        }
        let chunks = offset / block_size..(offset + len).div_ceil(block_size);
        self.load_chunks(device, chunks.clone()).await?;
        let clean = |index: u64| {
            !self.dirty.contains_key(&index)
                && !self.discarded(index)
                && self.committed_chunk(index).is_some()
        };
        if !chunks.into_iter().all(clean) {
            return Ok(None);
        }
        Ok(self.mapped(offset, len))
    }

    /// What the window is to map of `chunks` as of the last commit, in runs of chunks alike: a run
    /// of blocks of one extent, holes (zeros), or nothing, past the end.
    pub async fn runs(
        &mut self,
        device: &dyn Device,
        chunks: Range<u64>,
    ) -> Result<Extents, Error> {
        let committed = self.tree.counts[0];
        let known = chunks.start.min(committed)..chunks.end.min(committed);
        let fanout = self.tree.fanout;
        if !known.is_empty() {
            let nodes = known.start / fanout..(known.end - 1) / fanout + 1;
            self.tree.load(device, nodes.map(|index| (1, index))).await?;
        }
        let block_size = self.block_size() as u64;
        let contents = |index: u64| match (index < committed, self.tree.at((0, index))) {
            (false, _) => None,
            (true, None) => Some(Contents::Zeros),
            (true, Some(at)) => {
                Some(Contents::Extent { extent: at.extent, offset: at.block as u64 * block_size })
            }
        };
        let mut runs: Extents = Vec::new();
        for index in chunks {
            let next = contents(index);
            if let Some((run, last)) = runs.last_mut() {
                let follows = match (last, next) {
                    (None, None) | (Some(Contents::Zeros), Some(Contents::Zeros)) => true,
                    (
                        Some(Contents::Extent { extent, offset }),
                        Some(Contents::Extent { extent: next_extent, offset: next_offset }),
                    ) => {
                        *extent == next_extent
                            && *offset + (run.end - run.start) * block_size == next_offset
                    }
                    _ => false,
                };
                if follows {
                    run.end = index + 1;
                    continue;
                }
            }
            runs.push((index..index + 1, next));
        }
        Ok(runs)
    }

    /// Reads into `buf` from `offset`, as far as the contents go: how many bytes it read.
    pub async fn read(
        &mut self,
        device: &dyn Device,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, Error> {
        let end = (offset + buf.len() as u64).min(self.size).max(offset);
        let block_size = self.block_size() as u64;
        self.load_chunks(device, offset / block_size..end.div_ceil(block_size)).await?;
        let mut scratch = Vec::new();
        let mut at = offset;
        while at < end {
            let (index, within) = (at / block_size, (at % block_size) as usize);
            let on_device = |index| {
                if self.dirty.contains_key(&index)
                    || self.discarded(index)
                    || self.recent.contains_key(&index)
                    || self.mapped(index * block_size, block_size).is_some()
                {
                    return None;
                }
                self.committed_chunk(index)
            };
            if let Some(first) = on_device(index) {
                let mut blocks = 1;
                let limit = (TRANSFER_PIECE as u64 / block_size).max(1);
                while blocks < limit
                    && index + blocks < end.div_ceil(block_size)
                    && on_device(index + blocks).is_some_and(|next| {
                        next.extent == first.extent
                            && u64::from(next.block) == u64::from(first.block) + blocks
                    })
                {
                    blocks += 1;
                }
                let mut transfer = device.allocate_buffer((blocks * block_size) as usize).await;
                device
                    .read(first.extent, u64::from(first.block) * block_size, transfer.as_mut())
                    .await?;
                let len = (end - at).min(blocks * block_size - within as u64) as usize;
                transfer
                    .subslice(within..within + len)
                    .copy_to_slice(&mut buf[(at - offset) as usize..][..len]);
                at += len as u64;
                continue;
            }
            let len = ((block_size - within as u64).min(end - at)) as usize;
            let out = &mut buf[(at - offset) as usize..][..len];
            match self.dirty.get(&index) {
                Some(chunk) => out.copy_from_slice(&chunk[within..within + len]),
                None if self.discarded(index) => out.fill(0),
                None if len == block_size as usize => {
                    self.read_committed(device, index, out).await?
                }
                None => {
                    scratch.resize(block_size as usize, 0);
                    self.read_committed(device, index, &mut scratch).await?;
                    out.copy_from_slice(&scratch[within..within + len]);
                }
            }
            at += len as u64;
        }
        Ok((end - offset) as usize)
    }

    /// Chunk `index`, to be written: read in first, unless it is to be written whole.
    async fn dirty_chunk(
        &mut self,
        device: &dyn Device,
        index: u64,
        whole: bool,
    ) -> Result<&mut [u8], Error> {
        if !self.dirty.contains_key(&index) {
            let mut chunk = vec![0; self.block_size()].into_boxed_slice();
            if self.discarded(index) {
                self.undiscard(index);
            } else if !whole {
                self.load_chunks(device, index..index + 1).await?;
                self.read_committed(device, index, &mut chunk).await?;
            }
            self.dirty.insert(index, chunk);
        }
        Ok(self.dirty.get_mut(&index).unwrap())
    }

    /// Makes `chunks` holes, which read as zeros, those within the contents: a commit stores them
    /// as nothing, leaving what they held. What it notes is what the last commit holds data in, as
    /// ranges: chunks holes already cost nothing (TASK-584). It reads the nodes naming data
    /// among them.
    pub async fn discard(&mut self, device: &dyn Device, chunks: Range<u64>) -> Result<(), Error> {
        let end = chunks.end.min(self::chunks(self.size, self.block_size()));
        let chunks = chunks.start..end.max(chunks.start);
        let mut after = self.dirty.split_off(&chunks.start);
        self.dirty.append(&mut after.split_off(&chunks.end));
        let mut discarded: Vec<Range<u64>> = Vec::new();
        let held = chunks.start..chunks.end.min(self.cut);
        self.tree
            .visit(device, held, |(level, index), _| {
                if level > 0 {
                    return;
                }
                match discarded.last_mut() {
                    Some(range) if range.end == index => range.end += 1,
                    _ => discarded.push(index..index + 1),
                }
            })
            .await?;
        discarded.append(&mut self.holes);
        self.holes = merged(discarded);
        Ok(())
    }

    /// Writes `data` at `offset`.
    pub async fn write(
        &mut self,
        device: &dyn Device,
        offset: u64,
        data: &[u8],
    ) -> Result<(), Error> {
        let block_size = self.block_size() as u64;
        let end = offset + data.len() as u64;
        let mut at = offset;
        while at < end {
            let (index, within) = (at / block_size, (at % block_size) as usize);
            let len = ((block_size - within as u64).min(end - at)) as usize;
            let chunk = self.dirty_chunk(device, index, len == block_size as usize).await?;
            chunk[within..within + len].copy_from_slice(&data[(at - offset) as usize..][..len]);
            at += len as u64;
        }
        self.size = self.size.max(end);
        Ok(())
    }

    /// Makes the contents `size` bytes: what was past it, if they were longer, is gone, and reads
    /// as zeros if they grow again.
    pub async fn resize(&mut self, device: &dyn Device, size: u64) -> Result<(), Error> {
        if size < self.size {
            let kept = chunks(size, self.block_size());
            let within = (size % self.block_size() as u64) as usize;
            if within > 0 {
                self.dirty_chunk(device, kept - 1, false).await?[within..].fill(0);
            }
            self.dirty.split_off(&kept);
            self.holes.retain_mut(|range| {
                range.end = range.end.min(kept);
                !range.is_empty()
            });
            self.cut = self.cut.min(kept);
        }
        self.size = size;
        Ok(())
    }

    /// Plans the commit of what changed since the last: the extents to write, whose IDs `mint`
    /// gives, at most `max_blocks` each, and what it makes of the tree. It reads the nodes it
    /// rewrites and those of what it leaves, the owners of the extents it leaves blocks of, and
    /// the live blocks of the extents the last commit left sparse, which it writes again.
    pub async fn plan(
        &mut self,
        device: &dyn Device,
        mut mint: impl FnMut() -> ExtentId,
        max_blocks: u32,
    ) -> Result<Plan, Error> {
        let (block_size, fanout) = (self.block_size(), self.tree.fanout);
        let n = chunks(self.size, block_size);
        let counts = level_counts(n, fanout);
        let height = counts.len() as u32 - 1;
        let old_counts = self.tree.counts.clone();
        let (old_n, old_height) = (old_counts[0], self.tree.height());
        assert!(self.cut <= old_n.min(n), "cut at {} of {old_n} and {n}", self.cut);
        let in_new = |(level, index): Place| {
            level <= height && index < counts.get(level as usize).copied().unwrap_or(0)
        };

        // The chunks to write: those written, and the live ones of the extents left sparse, whose
        // live nodes are written again too; and those discarded, holes. The written ones borrowed,
        // not copied: a commit's data is copied once, into the plan (TASK-583).
        let mut rewritten: BTreeMap<u64, Box<[u8]>> = BTreeMap::new();
        let mut forced = BTreeSet::new();
        let mut owners_of =
            read_all_owners(device, self.committed.sparse.iter().copied(), block_size).await?;
        let sparse_owned: Vec<(ExtentId, u32, Place)> = (self.committed.sparse.iter())
            .flat_map(|(extent, _)| {
                let owners = &owners_of[extent];
                (0..)
                    .zip(owners)
                    .map(|(block, owner)| (*extent, block, (owner.level(), owner.index())))
            })
            .collect();
        let places: Vec<Place> = sparse_owned.iter().map(|(_, _, place)| *place).collect();
        let found = self.tree.lookup(device, &places).await?;
        for ((extent, block, (level, index)), at) in sparse_owned.into_iter().zip(found) {
            let live = at.is_some_and(|at| at.extent == extent && at.block == block);
            if !live {
                continue;
            }
            if level == 0 {
                if index < self.cut
                    && !self.dirty.contains_key(&index)
                    && !self.discarded(index)
                    && !rewritten.contains_key(&index)
                {
                    let mut chunk = vec![0; block_size].into_boxed_slice();
                    self.read_committed(device, index, &mut chunk).await?;
                    rewritten.insert(index, chunk);
                }
            } else if in_new((level, index)) {
                forced.insert((level, index));
            }
        }
        let holes = self.holes.iter().flat_map(|range| range.clone().map(|index| (index, None)));
        let written: BTreeMap<u64, Option<&[u8]>> = (self.dirty.iter().chain(&rewritten))
            .map(|(index, chunk)| (*index, Some(&chunk[..])))
            .chain(holes)
            .collect();

        // The nodes to write: above each chunk written, along the end's path if the tree's shape
        // changed, above chunks the last commit had that are cut, and the live nodes of sparse
        // extents, with what is above them. Not above chunks past what the last commit had: holes
        // there and then, which only the nodes above them as written name.
        let mut rebuilt = BTreeSet::new();
        let above = |rebuilt: &mut BTreeSet<Place>, level: u32, index: u64| {
            let mut index = index;
            for at in level..=height {
                rebuilt.insert((at, index));
                index /= fanout;
            }
        };
        for index in written.keys() {
            above(&mut rebuilt, 1, index / fanout);
        }
        if n > 0 && (n != old_n || height != old_height || self.cut < old_n) {
            above(&mut rebuilt, 1, (n - 1) / fanout);
        }
        let mut left = BTreeSet::new();
        self.tree
            .visit(device, self.cut..u64::MAX, |place @ (level, index), _| {
                if !in_new(place) || level == 0 {
                    left.insert(place);
                }
                if level == 0 && index < n {
                    above(&mut rebuilt, 1, index / fanout);
                }
            })
            .await?;
        for (level, index) in forced {
            above(&mut rebuilt, level, index);
        }
        // A tree grown taller keeps its old top as the first node of its level: the nodes above
        // it, new, name it. Grown by a level, the end's path holds them; by more, it need not.
        if height > old_height && old_height > 0 {
            above(&mut rebuilt, old_height + 1, 0);
        }

        // What the commit replaces or leaves: everything it places, the chunks cut, and the nodes
        // past its shape. Read, with what the nodes rewritten name now.
        left.extend(written.keys().map(|index| (0, *index)));
        left.extend(rebuilt.iter().copied());
        let parents: Vec<Place> = (left.iter().map(|place| self.tree.parent(*place)))
            .chain(rebuilt.iter().copied())
            .collect();
        self.tree.load(device, parents).await?;
        let released: Vec<(Place, Ref)> =
            (left.iter()).filter_map(|place| self.tree.at(*place).map(|at| (*place, at))).collect();

        // The extents it leaves blocks of, and how many of their blocks live on after it: the
        // owners the tree still names there, which it neither places nor leaves.
        let mut touched: BTreeMap<ExtentId, u32> = BTreeMap::new();
        for (_, at) in &released {
            touched.insert(at.extent, at.blocks);
        }
        let unread = touched.iter().filter(|(extent, _)| !owners_of.contains_key(extent));
        let unread: Vec<_> = unread.map(|(extent, blocks)| (*extent, *blocks)).collect();
        owners_of.extend(read_all_owners(device, unread, block_size).await?);
        let survivors: Vec<(ExtentId, u32, Place)> = (touched.keys())
            .flat_map(|extent| {
                let owners = &owners_of[extent];
                (0..)
                    .zip(owners)
                    .map(|(block, owner)| (*extent, block, (owner.level(), owner.index())))
            })
            .filter(|(_, _, place)| !left.contains(place))
            .collect();
        let places: Vec<Place> = survivors.iter().map(|(_, _, place)| *place).collect();
        let found = self.tree.lookup(device, &places).await?;
        let mut live: HashMap<ExtentId, u32> = HashMap::new();
        for ((extent, block, _), at) in survivors.into_iter().zip(found) {
            if at.is_some_and(|at| at.extent == extent && at.block == block) {
                *live.entry(extent).or_default() += 1;
            }
        }
        let mut dead = Vec::new();
        let mut sparse = Vec::new();
        for (&extent, &blocks) in &touched {
            match live.get(&extent).copied().unwrap_or(0) {
                0 => dead.push(extent),
                live if (live as u64) * 2 < blocks as u64 => sparse.push((extent, blocks)),
                _ => {}
            }
        }
        sparse.truncate(SPARSE_KEPT);

        // Which of the nodes to write hold anything, bottom up: a node of holes is a hole.
        let tree = &self.tree;
        let cut = self.cut;
        let committed_chunk = |index: u64| (index < cut).then(|| tree.at((0, index))).flatten();
        let mut holds = HashMap::new();
        for &(level, index) in &rebuilt {
            let children = index * fanout..((index + 1) * fanout).min(counts[level as usize - 1]);
            let child_holds = |child: u64| match level - 1 {
                0 => match written.get(&child) {
                    Some(chunk) => chunk.is_some(),
                    None => committed_chunk(child).is_some(),
                },
                below => match holds.get(&(below, child)) {
                    Some(held) => *held,
                    None => tree.at((below, child)).is_some(),
                },
            };
            let held = children.into_iter().any(child_holds);
            holds.insert((level, index), held);
        }

        // Where each block goes: the chunks, then the nodes, level by level, in extents of at most
        // `max_blocks`, each followed by its blocks' owners.
        let chunks_written = written.values().filter(|chunk| chunk.is_some()).count();
        let total = chunks_written + holds.values().filter(|held| **held).count();
        let max_blocks = max_blocks as usize;
        let extents: Vec<(ExtentId, u32)> = (0..total.div_ceil(max_blocks))
            .map(|extent| (mint(), (total - extent * max_blocks).min(max_blocks) as u32))
            .collect();
        let mut data = vec![0; total * block_size];
        let mut owners = Vec::with_capacity(total);
        let mut placed: BTreeMap<Place, Option<usize>> = BTreeMap::new();
        let place_of = |position: usize| {
            let (extent, blocks) = extents[position / max_blocks];
            Ref { extent, block: (position % max_blocks) as u32, blocks }
        };
        let mut next = 0;
        for (index, chunk) in &written {
            let Some(chunk) = chunk else {
                placed.insert((0, *index), None);
                continue;
            };
            data[next * block_size..][..block_size].copy_from_slice(chunk);
            placed.insert((0, *index), Some(next));
            owners.push(Owner::new(0, *index));
            next += 1;
        }
        // Where child `child` of level `level` is once the commit is made.
        let after = |placed: &BTreeMap<Place, Option<usize>>, level: u32, child: u64| match placed
            .get(&(level, child))
        {
            Some(position) => position.map(place_of),
            None => match level {
                0 => committed_chunk(child),
                _ => tree.at((level, child)),
            },
        };
        let mut nodes: Vec<(Place, Option<Node>)> = Vec::with_capacity(rebuilt.len());
        for &(level, index) in &rebuilt {
            if !holds[&(level, index)] {
                placed.insert((level, index), None);
                nodes.push(((level, index), None));
                continue;
            }
            let children = index * fanout..((index + 1) * fanout).min(counts[level as usize - 1]);
            let refs: Vec<_> = children.map(|child| after(&placed, level - 1, child)).collect();
            format::encode_node(&mut data[next * block_size..][..block_size], level, index, &refs);
            let mut node: Node = vec![None; fanout as usize].into();
            node[..refs.len()].copy_from_slice(&refs);
            nodes.push(((level, index), Some(node)));
            placed.insert((level, index), Some(next));
            owners.push(Owner::new(level, index));
            next += 1;
        }
        assert_eq!(next, total);
        let top = match height {
            0 => None,
            height => after(&placed, height, 0),
        };

        Ok(Plan {
            data,
            owners,
            extents,
            max_blocks,
            changed: placed
                .keys()
                .filter(|(level, _)| *level == 0)
                .map(|(_, index)| *index)
                .collect(),
            nodes,
            counts,
            root: TreeRoot { size: self.size, height, top, sparse },
            dead,
        })
    }

    /// Makes `plan`, written and committed, the last commit: what it changed of what the
    /// window's region maps is stale ([`Pages::mark_stale`]), to be mapped again.
    pub fn apply(&mut self, plan: Plan) {
        let Plan { changed: placed_chunks, nodes, counts, root, .. } = plan;
        let cut = self.cut;
        let old_n = self.tree.counts[0];
        let mut changed: Vec<Range<u64>> = Vec::new();
        for index in placed_chunks {
            match changed.last_mut() {
                Some(range) if range.end == index => range.end += 1,
                _ => changed.push(index..index + 1),
            }
        }
        // Cut, or past the end.
        if cut < old_n {
            changed.push(cut..old_n);
        }
        let n = counts[0];
        let height = counts.len() as u32 - 1;
        let tree = &mut self.tree;
        tree.nodes.retain(|(level, index), _| {
            *level <= height && *index < counts.get(*level as usize).copied().unwrap_or(0)
        });
        for (place, node) in nodes {
            match node {
                Some(node) => tree.nodes.insert(place, node),
                None => tree.nodes.remove(&place),
            };
        }
        (tree.top, tree.counts) = (root.top, counts);
        self.committed = root;
        self.cut = n;
        let dirty = std::mem::take(&mut self.dirty);
        let holes = std::mem::take(&mut self.holes);
        if self.region.is_none() {
            return;
        }
        self.mark_stale(merged(changed));
        // What it wrote of what the region maps as before it, to read until mapped again: of the
        // chunks it holds a copy of already, and others while it holds fewer than REMAP_BATCH.
        self.recent.retain(|index, _| *index < cut.min(n));
        let zeros = || vec![0; self.tree.block_size].into_boxed_slice();
        let discarded = holes.into_iter().flatten().filter(|index| self.is_stale(*index));
        let discarded: Vec<_> = discarded.map(|index| (index, zeros())).collect();
        for (index, chunk) in dirty.into_iter().chain(discarded) {
            let kept = self.recent.len() < REMAP_BATCH as usize || self.recent.contains_key(&index);
            if index < n && self.is_stale(index) && kept {
                self.recent.insert(index, chunk);
            }
        }
    }
}

/// A commit of the contents, planned ([`Pages::plan`]).
pub(crate) struct Plan {
    /// The blocks to write, in order, the extents' one after another.
    data: Vec<u8>,
    /// What each of them holds.
    owners: Vec<Owner>,
    /// The extents to write, and the blocks each holds.
    extents: Vec<(ExtentId, u32)>,
    max_blocks: usize,
    /// The chunks it writes.
    changed: Vec<u64>,
    /// The nodes it writes, their children; or none, a node of holes.
    nodes: Vec<(Place, Option<Node>)>,
    /// The counts of the tree's levels.
    counts: Vec<u64>,
    /// The contents, as the commit leaves them.
    pub root: TreeRoot,
    /// The extents the commit leaves unreferenced.
    pub dead: Vec<ExtentId>,
}

impl Plan {
    /// The extents it writes.
    pub fn extents(&self) -> impl Iterator<Item = ExtentId> + '_ {
        self.extents.iter().map(|(extent, _)| *extent)
    }

    /// Writes its extents to `device`, each its blocks then their owners, the last write
    /// committing `root`, if given, as the device's root, its barrier sealing them: one request,
    /// not two. With no extents to write, the root is committed alone.
    pub async fn write(&self, device: &dyn Device, root: Option<&[u8]>) -> Result<(), Failure> {
        let block_size = device.block_size() as usize;
        // Each extent's blocks, from the plan's data, and then their owners, in pieces: copied
        // into the device's buffers alone (TASK-583).
        let extents: Vec<(ExtentId, &[u8], Vec<u8>)> = (self.extents.iter().enumerate())
            .map(|(at, (extent, blocks))| {
                let (first, blocks) = (at * self.max_blocks, *blocks as usize);
                let data = &self.data[first * block_size..][..blocks * block_size];
                let owners = &self.owners[first..first + blocks];
                (*extent, data, format::encode_owners(owners, block_size))
            })
            .collect();
        let writes: Vec<(usize, Range<usize>)> = (extents.iter().enumerate())
            .flat_map(|(at, (_, data, owners))| {
                let len = data.len() + owners.len();
                (0..len)
                    .step_by(TRANSFER_PIECE)
                    .map(move |from| (at, from..len.min(from + TRANSFER_PIECE)))
            })
            .collect();
        let last = writes.len().checked_sub(1);
        for (at, (index, piece)) in writes.into_iter().enumerate() {
            let (extent, data, owners) = &extents[index];
            let (extent, offset) = (*extent, piece.start as u64);
            let mut buffer = device.allocate_buffer(piece.len()).await;
            // The piece's part of the blocks, then of the owners that follow them.
            let mut filled = 0;
            for (source, starts) in [(*data, 0), (&owners[..], data.len())] {
                let part = piece.start.max(starts)..piece.end.min(starts + source.len());
                if !part.is_empty() {
                    let len = part.len();
                    (buffer.subslice_mut(filled..filled + len))
                        .copy_from_slice(&source[part.start - starts..part.end - starts]);
                    filled += len;
                }
            }
            let commits = root.filter(|_| Some(at) == last);
            let options = WriteOptions {
                flags: commits.map_or(WriteFlags::empty(), |_| WriteFlags::POST_BARRIER),
                root: commits,
                ..Default::default()
            };
            let written = device.write_with_opts(extent, offset, buffer.as_ref(), options).await;
            written.map_err(|error| match commits {
                Some(_) => Failure::Committing(error),
                None => Failure::Writing(error.context(format!("writing {extent:?} at {offset}"))),
            })?;
        }
        match (root, last) {
            (Some(root), None) => device.commit_root(root).await.map_err(Failure::Committing),
            _ => Ok(()),
        }
    }
}

/// How a plan's writing failed ([`Plan::write`]).
#[derive(Debug)]
pub(crate) enum Failure {
    /// Writing its extents, before any barrier: the device holds what it held, and what it wrote,
    /// which no root names.
    Writing(Error),
    /// Committing the root: whether the device holds it, its next opening finds.
    Committing(Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use rand::{Rng as _, RngExt as _, SeedableRng as _};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use storage_device::fake_device::{FakeDevice, Op};

    const BLOCK: usize = 512;

    /// Pages on a device, committed as a block device commits them, and a model of its bytes.
    struct Model {
        device: FakeDevice,
        pages: Pages,
        model: Vec<u8>,
        next: u64,
        max_blocks: u32,
        /// The device's reads so far.
        reads: Arc<AtomicU64>,
    }

    impl Model {
        fn new(max_blocks: u32) -> Self {
            let mut device = FakeDevice::new(1 << 20, BLOCK as u32).calm();
            let reads = Arc::new(AtomicU64::new(0));
            let counted = reads.clone();
            device.set_op_callback(move |op| {
                if matches!(op, Op::Read) {
                    counted.fetch_add(1, Ordering::Relaxed);
                }
                Ok(())
            });
            let pages = Pages::new(BLOCK);
            Self { device, pages, model: Vec::new(), next: 0, max_blocks, reads }
        }

        fn write(&mut self, offset: u64, data: &[u8]) {
            block_on(self.pages.write(&self.device, offset, data)).unwrap();
            let end = offset as usize + data.len();
            if self.model.len() < end {
                self.model.resize(end, 0);
            }
            self.model[offset as usize..end].copy_from_slice(data);
        }

        fn resize(&mut self, size: u64) {
            block_on(self.pages.resize(&self.device, size)).unwrap();
            self.model.resize(size as usize, 0);
        }

        fn discard(&mut self, chunks: Range<u64>) {
            block_on(self.pages.discard(&self.device, chunks.clone())).unwrap();
            let end = (chunks.end * BLOCK as u64).min(self.model.len() as u64) as usize;
            let start = ((chunks.start * BLOCK as u64) as usize).min(end);
            self.model[start..end].fill(0);
        }

        fn commit(&mut self) {
            let next = &mut self.next;
            let mint = || {
                *next += 1;
                ExtentId::new(storage_device::fake_device::DEFAULT_CELL, *next)
            };
            let plan = block_on(self.pages.plan(&self.device, mint, self.max_blocks)).unwrap();
            block_on(plan.write(&self.device, None)).unwrap();
            for extent in &plan.dead {
                block_on(self.device.free(*extent)).unwrap();
            }
            block_on(self.device.flush()).unwrap();
            self.pages.apply(plan);
            assert!(!self.pages.changed());
        }

        /// The pages opened again from the last commit's root, as a block device's next opening
        /// opens them: what it wrote since is lost.
        fn reopen(&mut self) {
            self.pages = Pages::open(BLOCK, &self.pages.root()).unwrap();
            let size = self.pages.size() as usize;
            self.model.resize(size, 0);
        }

        /// Checks what the pages read, and, committed, what they read once opened again from
        /// their root, and that the extents the device holds are those the tree names.
        fn check(&mut self) {
            assert_eq!(read(&self.device, &mut self.pages), self.model);
            let mut opened = Pages::open(BLOCK, &self.pages.root()).unwrap();
            assert_eq!(opened.size(), self.pages.root().size);
            if !self.pages.changed() {
                assert_eq!(read(&self.device, &mut opened), self.model);
                let held: Vec<_> =
                    block_on(opened.extents(&self.device)).unwrap().into_iter().collect();
                assert_eq!(held, self.device.extents(), "the extents the device holds");
            }
        }

        fn reads(&self) -> u64 {
            self.reads.load(Ordering::Relaxed)
        }
    }

    fn read(device: &FakeDevice, pages: &mut Pages) -> Vec<u8> {
        let mut out = vec![0xee; pages.size() as usize + 100];
        let read = block_on(pages.read(device, 0, &mut out)).unwrap();
        out.truncate(read);
        out
    }

    #[test]
    fn levels_grow_by_the_fanout() {
        assert_eq!(level_counts(0, 20), vec![0]);
        assert_eq!(level_counts(1, 20), vec![1, 1]);
        assert_eq!(level_counts(20, 20), vec![20, 1]);
        assert_eq!(level_counts(21, 20), vec![21, 2, 1]);
        assert_eq!(level_counts(401, 20), vec![401, 21, 2, 1]);
    }

    #[test]
    fn pages_read_back_through_commits_and_openings() {
        let mut model = Model::new(8);
        model.write(0, &[1; 100]);
        model.check();
        model.commit();
        model.check();
        // Past several levels of the tree (the fanout is 20), and partly over what was there.
        model.write(50, &vec![2; BLOCK * 450]);
        model.commit();
        model.check();
        model.reopen();
        model.write(BLOCK as u64 * 3 + 7, &[3; 9]);
        model.check();
        model.commit();
        model.check();
    }

    /// A tree grown two levels taller in one commit keeps what it held: the node between its old
    /// top and its new one is written, not left a hole, which lost the first chunks on its
    /// next opening. The eager tree wrote every node from the cut to the end, this one among them
    /// (TASK-440).
    #[test]
    fn a_tree_grown_two_levels_keeps_its_old_top() {
        let mut model = Model::new(64);
        model.write(0, &vec![1; BLOCK * 424]);
        model.commit();
        model.resize(BLOCK as u64 * 20);
        model.commit();
        assert_eq!(model.pages.root().height, 1);
        model.write(BLOCK as u64 * 494, &[2; BLOCK]);
        model.commit();
        assert_eq!(model.pages.root().height, 3);
        model.check();
    }

    /// Discarded chunks read as zeros, before their commit and after an opening, and the commit
    /// stores nothing for them: contents of nothing but holes hold no extent (TASK-456.2).
    #[test]
    fn discarded_chunks_are_holes_stored_as_nothing() {
        let mut model = Model::new(64);
        model.write(0, &vec![5; BLOCK * 40]);
        model.commit();
        model.discard(2..30);
        model.check();
        model.commit();
        model.check();
        model.reopen();
        model.check();
        model.discard(0..40);
        model.commit();
        model.check();
        assert_eq!((model.pages.size(), model.device.extents()), (BLOCK as u64 * 40, Vec::new()));
    }

    #[test]
    fn shrinking_cuts_what_follows() {
        let mut model = Model::new(64);
        model.write(0, &vec![4; BLOCK * 30]);
        model.commit();
        model.resize(BLOCK as u64 * 2 + 10);
        model.check();
        model.resize(BLOCK as u64 * 25);
        model.check();
        model.write(BLOCK as u64 * 24, &[5; 3]);
        model.commit();
        model.check();
        model.reopen();
        model.resize(0);
        model.commit();
        model.check();
        assert_eq!(model.device.extents(), Vec::new());
    }

    /// Opening the pages reads nothing; a read of one chunk reads the nodes on its path and the
    /// chunk, and holds those nodes alone (TASK-440).
    #[test]
    fn an_opening_reads_nothing_and_a_read_reads_one_path() {
        let mut model = Model::new(1024);
        model.write(0, &vec![6; BLOCK * 401]);
        model.commit();
        assert_eq!(model.pages.root().height, 3);
        let before = model.reads();
        let mut opened = Pages::open(BLOCK, &model.pages.root()).unwrap();
        assert_eq!((model.reads() - before, opened.nodes_held()), (0, 0));
        let mut chunk = vec![0; BLOCK];
        let read = block_on(opened.read(&model.device, BLOCK as u64 * 300, &mut chunk)).unwrap();
        assert_eq!((read, chunk), (BLOCK, vec![6; BLOCK]));
        assert_eq!((model.reads() - before, opened.nodes_held()), (4, 3));
    }

    /// Contents of 64 GiB written in three places holds the nodes of their paths alone, written
    /// and read back across an opening (TASK-440).
    #[test]
    fn a_sparse_64_gib_device_holds_what_it_touched() {
        let mut model = Model::new(64);
        let size = 64u64 << 30;
        let places = [0, size / 2 + 12345, size - 3];
        let mut pages = Pages::new(BLOCK);
        for (at, place) in places.iter().enumerate() {
            block_on(pages.write(&model.device, *place, &[at as u8 + 1; 3])).unwrap();
        }
        model.pages = pages;
        model.commit();
        let height = model.pages.root().height as usize;
        assert_eq!(height, 7);
        assert!(model.pages.nodes_held() <= 3 * height, "{} nodes", model.pages.nodes_held());
        let mut opened = Pages::open(BLOCK, &model.pages.root()).unwrap();
        for (at, place) in places.iter().enumerate() {
            let mut out = [0; 3];
            block_on(opened.read(&model.device, *place, &mut out)).unwrap();
            assert_eq!(out, [at as u8 + 1; 3]);
        }
        assert!(opened.nodes_held() <= 3 * height, "{} nodes", opened.nodes_held());
        let mut hole = [0xee; 8];
        block_on(opened.read(&model.device, size / 4, &mut hole)).unwrap();
        assert_eq!(hole, [0; 8]);
        // The three chunks, and the nodes of their paths: no more.
        let stored: u64 = (model.device.extents().iter())
            .map(|extent| model.device.extent_len(*extent).unwrap())
            .sum();
        assert!(stored <= (3 + 3 * height as u64 + 2) * BLOCK as u64, "{stored} bytes stored");
    }

    /// An extent a commit leaves less than half live is named in the root, and the next commit,
    /// after an opening, writes its live blocks again and frees it (TASK-440).
    #[test]
    fn a_sparse_extent_is_compacted_after_an_opening() {
        let mut model = Model::new(8);
        model.write(0, &vec![7; BLOCK * 8]);
        model.commit();
        let first = model.device.extents()[0];
        model.write(0, &vec![8; BLOCK * 5]);
        model.commit();
        assert_eq!(model.pages.root().sparse, vec![(first, 8)]);
        model.check();
        model.reopen();
        model.write(BLOCK as u64 * 20, &[9; 1]);
        model.commit();
        assert!(!model.device.extents().contains(&first), "{first:?} still held");
        assert_eq!(model.pages.root().sparse, Vec::new());
        model.check();
    }

    /// Random writes and resizes, committed and opened again at random, read back as a model
    /// of the bytes does, and the device holds the extents the tree names, once committed.
    #[test]
    fn random_changes_read_as_the_model() {
        for seed in 0..100 {
            let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
            let mut model = Model::new(rng.random_range(1..16));
            for _ in 0..200 {
                match rng.random_range(0..13) {
                    0 => model.resize(rng.random_range(0..BLOCK as u64 * 500)),
                    11 => {
                        let start = rng.random_range(0..500);
                        model.discard(start..start + rng.random_range(1..40));
                    }
                    1..4 => model.commit(),
                    4 => {
                        model.commit();
                        model.reopen();
                    }
                    _ => {
                        let offset = rng.random_range(0..BLOCK as u64 * 500);
                        let mut data = vec![0; rng.random_range(1..BLOCK * 3)];
                        rng.fill_bytes(&mut data);
                        model.write(offset, &data);
                    }
                }
                if rng.random_range(0..20) == 0 {
                    model.check();
                }
            }
            model.commit();
            model.check();
        }
    }

    /// TASK-552: a device whose window maps a chunk that each commit rewrites, as a database's
    /// first page, and that grows a chunk a commit, never read back, keeps no more of what its
    /// commits wrote than a remap batch, however many commits pass with nothing mapped again: it
    /// kept a copy of every chunk written, past the region's end too.
    #[test]
    fn what_a_mapped_device_keeps_of_its_commits_stays_within_a_remap_batch() {
        let mut model = Model::new(4);
        let chunks = 64;
        let mut memory = vec![0u8; chunks * BLOCK];
        let base = std::ptr::NonNull::new(memory.as_mut_ptr()).unwrap();
        model.pages.set_region(Some((base, memory.len() as u64)));
        model.write(0, &[1; BLOCK]);
        model.commit();
        // The region maps chunk 0, as a read of the database's header maps it.
        memory[..BLOCK].fill(1);
        model.pages.mark_mapped([(0..1, true)]);
        for n in 1..4000u64 {
            model.write(0, &[n as u8; BLOCK]);
            model.write(n * BLOCK as u64, &[n as u8; BLOCK]);
            model.commit();
            let kept = model.pages.recent.len();
            assert!(kept <= REMAP_BATCH as usize, "{kept} chunks kept after commit {n}");
        }
        assert_eq!(model.pages.recent.keys().collect::<Vec<_>>(), [&0]);
        model.check();
    }

    /// TASK-584: discarding what the last commit holds as holes already changes nothing, and a
    /// discard of what it holds data in, or of what was written since, reads as zeros, written
    /// again in part after.
    #[test]
    fn discarding_holes_already_holes_changes_nothing() {
        let mut model = Model::new(4);
        model.resize(4096 * BLOCK as u64);
        model.commit();
        model.discard(0..4096);
        assert!(!model.pages.changed(), "a discard of holes changed the contents");
        assert!(model.pages.holes.is_empty());
        model.write(10 * BLOCK as u64, &vec![7; 20 * BLOCK]);
        model.commit();
        model.write(25 * BLOCK as u64, &vec![8; 10 * BLOCK]);
        model.discard(0..4096);
        assert_eq!(model.pages.holes, [10..30]);
        assert!(model.pages.dirty.is_empty());
        model.write(12 * BLOCK as u64 + 3, &[9; 5]);
        assert_eq!(model.pages.holes, [10..12, 13..30]);
        model.check();
        model.commit();
        model.check();
    }
}
