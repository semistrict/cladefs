// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What a block device keeps on its extent device: the root, and the nodes of its tree.
//!
//! **Refs.** A block of an extent is named by a ref of 24 bytes: the extent's cell and sequence
//! number, the block's index in it, and how many blocks the extent holds, which no ref to an
//! extent says otherwise; none, a ref of zeros, is a hole.
//!
//! **Versions.** The family is frozen (doc-28): every later build reads every version of
//! [`FORMAT`]'s range. Each object an extent holds says its version, as it outlives the root
//! that wrote it: a commit leaves the blocks it doesn't rewrite where they are, under a root of
//! a newer version, and a fork reads its parent's. The root, a node's trailer and an owners'
//! list each say theirs; a list of dying extents is read by its root's, which alone names it.
//!
//! **Nodes.** A node is a block of refs, `fanout` of them, then a trailer: [`NODE_MAGIC`], its
//! version, and its place, its level and its index at that level, packed as an [`Owner`], which
//! its reader checks against where it looked for it.
//!
//! **Owners.** An extent a commit writes holds its blocks, then their owners: a version and how
//! many owners follow, then what each block holds, a chunk (level 0) or a node, as its level and
//! index packed in 8 bytes ([`Owner`]), padded to whole blocks ([`owner_blocks`]). A ref's
//! `blocks` counts the blocks alone. A block is live while the tree names it where its owner
//! says; a commit reads the owners of the extents it leaves blocks of, to know which live on
//! (TASK-440).
//!
//! **The root.** [`ROOT_MAGIC`], its version, the sequence number minting goes on from, the extents
//! to free once the root is durable, and the extent listing more of them, if any, and the tree
//! ([`TreeRoot`]): the contents' size, the tree's height, the ref to its top node, and the extents
//! the last commit left less than half live, to be compacted by the next, at most
//! [`SPARSE_KEPT`]. An extent device's root is small ([`storage_device::MAX_ROOT`], the extent
//! daemon's 4 KiB), and the root at its largest fits it ([`MAX_ROOT_BYTES`]).

use anyhow::{Context as _, Error, ensure};
use storage_device::ExtentId;

/// The bytes of a ref.
const REF_BYTES: usize = 24;

/// The bytes of a node's trailer.
const TRAILER_BYTES: usize = 16;

/// What a node's trailer starts with.
const NODE_MAGIC: u32 = u32::from_le_bytes(*b"FXSN");

/// What a block device's root starts with.
const ROOT_MAGIC: &[u8; 8] = b"CLADEBLK";

/// The family, frozen at 2 (doc-28, board 10052): 1, one tree (board 5790); 2, nodes and owners'
/// lists saying their version.
pub(crate) const FORMAT: formats::Format = formats::Format::new("block device", 2, 2);

/// The bytes before an extent's owners: their version, and how many follow.
const OWNERS_HEADER: usize = 8;

/// The most extents the tree names to compact at the next commit: more are left as they are until
/// a later commit leaves blocks of them again.
pub(crate) const SPARSE_KEPT: usize = 16;

/// The most dying extents a root names itself: more are listed in an extent of their own.
pub(crate) const DYING_KEPT: usize = 64;

/// The most dying extents a list names: 16 MiB of them, which a flush leaving more dying fails
/// before it writes anything, and a root read back naming more is refused (board 8549). Within
/// the extent daemon's largest extent (doc-4's `max_extent`, 64 MiB), and within a daemon
/// device's transfer buffers (`cladefs::DaemonDevice`'s 32 MiB), whose allocation of more panics
/// (hotbed review 57). A flush of 4 KiB blocks in extents of 1024 leaves that many dying having
/// discarded 4 TiB.
pub(crate) const MAX_LISTED: u64 = (16 << 20) / 16;

/// The bytes of the largest root: [`DYING_KEPT`] dying extents and [`SPARSE_KEPT`] to compact.
pub(crate) const MAX_ROOT_BYTES: usize = ROOT_MAGIC.len()
    + 4
    + 8
    + 8
    + DYING_KEPT * 16
    + 24
    + (8 + 4 + REF_BYTES + 8 + SPARSE_KEPT * 20);

/// The bytes of a block's owner in an extent's list of them.
const OWNER_BYTES: usize = 8;

/// What a block of an extent holds: chunk `index` at level 0, or node `index` of `level`, packed.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Owner(u64);

/// By its level and index, as the corpus's cases say it (doc-28): never otherwise.
impl std::fmt::Debug for Owner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Owner").field("level", &self.level()).field("index", &self.index()).finish()
    }
}

impl Owner {
    const LEVEL_SHIFT: u32 = 58;

    pub fn new(level: u32, index: u64) -> Self {
        assert!(index >> Self::LEVEL_SHIFT == 0 && level < 63, "node {index} of level {level}");
        Self((level as u64) << Self::LEVEL_SHIFT | index)
    }

    pub fn level(self) -> u32 {
        (self.0 >> Self::LEVEL_SHIFT) as u32
    }

    pub fn index(self) -> u64 {
        self.0 & ((1 << Self::LEVEL_SHIFT) - 1)
    }
}

/// The blocks after an extent's `blocks` that list their owners, in blocks of `block_size`.
pub(crate) fn owner_blocks(blocks: u32, block_size: usize) -> u32 {
    (OWNERS_HEADER + blocks as usize * OWNER_BYTES).div_ceil(block_size) as u32
}

/// `owners`' list, padded to whole blocks of `block_size`.
pub(crate) fn encode_owners(owners: &[Owner], block_size: usize) -> Vec<u8> {
    let count = u32::try_from(owners.len()).expect("an extent's blocks number a u32");
    let mut out = Vec::with_capacity(owner_blocks(count, block_size) as usize * block_size);
    out.extend_from_slice(&FORMAT.current.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend(owners.iter().flat_map(|owner| owner.0.to_le_bytes()));
    out.resize(owner_blocks(count, block_size) as usize * block_size, 0);
    out
}

/// The `count` owners `bytes`, an extent's list of them, lists. Untrusted, as the root is.
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub(crate) fn decode_owners(bytes: &[u8], count: u32) -> Result<Vec<Owner>, Error> {
    let mut reader = Reader(bytes);
    FORMAT.check(reader.u32().context("the owners' list ends early")?)?;
    let listed = reader.u32().context("the owners' list ends early")?;
    ensure!(listed == count, "a list of {listed} owners where {count} were looked for");
    let owners = (0..count).map(|_| Ok(Owner(reader.u64()?)));
    owners.collect::<Result<_, Error>>().context("the owners' list ends early")
}

/// A block of an extent: what a tree's refs name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ref {
    pub extent: ExtentId,
    /// The block's index in the extent.
    pub block: u32,
    /// The blocks the extent holds.
    pub blocks: u32,
}

/// The refs a node of `block_size` bytes holds.
pub(crate) fn fanout(block_size: usize) -> usize {
    (block_size - TRAILER_BYTES) / REF_BYTES
}

/// Fails unless blocks of `block_size` bytes hold nodes of two refs at least.
pub(crate) fn check_block_size(block_size: usize) -> Result<(), Error> {
    ensure!(
        block_size >= TRAILER_BYTES + 2 * REF_BYTES,
        "blocks of {block_size} bytes hold no node"
    );
    Ok(())
}

/// Writes node `index` of `level`, whose refs are `refs`, at most a fanout of them, into `block`.
pub(crate) fn encode_node(block: &mut [u8], level: u32, index: u64, refs: &[Option<Ref>]) {
    assert!(refs.len() <= fanout(block.len()), "{} refs in a node", refs.len());
    block.fill(0);
    for (at, slot) in refs.iter().zip(block.as_chunks_mut::<REF_BYTES>().0) {
        encode_ref(slot, *at);
    }
    let len = block.len();
    let trailer = &mut block[len - TRAILER_BYTES..];
    trailer[0..4].copy_from_slice(&NODE_MAGIC.to_le_bytes());
    trailer[4..8].copy_from_slice(&FORMAT.current.to_le_bytes());
    trailer[8..16].copy_from_slice(&Owner::new(level, index).0.to_le_bytes());
}

/// The first `count` refs of node `index` of `level`, read as `block`.
pub(crate) fn decode_node(
    block: &[u8],
    level: u32,
    index: u64,
    count: usize,
) -> Result<Vec<Option<Ref>>, Error> {
    ensure!(count <= fanout(block.len()), "{count} refs in a node");
    let trailer = &block[block.len() - TRAILER_BYTES..];
    let magic = u32::from_le_bytes(trailer[0..4].try_into().unwrap());
    let found = Owner(u64::from_le_bytes(trailer[8..16].try_into().unwrap()));
    ensure!(
        magic == NODE_MAGIC,
        "node {index} of level {level} reads as no node (magic {magic:#x})"
    );
    let version = u32::from_le_bytes(trailer[4..8].try_into().unwrap());
    FORMAT.check(version).with_context(|| format!("node {index} of level {level}"))?;
    ensure!(
        found == Owner::new(level, index),
        "node {index} of level {level} reads as node {} of level {}",
        found.index(),
        found.level()
    );
    block.as_chunks::<REF_BYTES>().0.iter().take(count).map(decode_ref).collect()
}

/// Writes `at` into `slot`, a ref's bytes: zeros if none.
fn encode_ref(slot: &mut [u8; REF_BYTES], at: Option<Ref>) {
    let at = at.unwrap_or(Ref { extent: ExtentId::new(0, 0), block: 0, blocks: 0 });
    slot[0..8].copy_from_slice(&at.extent.cell.to_le_bytes());
    slot[8..16].copy_from_slice(&at.extent.sequence.to_le_bytes());
    slot[16..20].copy_from_slice(&at.block.to_le_bytes());
    slot[20..24].copy_from_slice(&at.blocks.to_le_bytes());
}

/// The ref `slot`, a ref's bytes, holds: none if its extent holds no blocks.
fn decode_ref(slot: &[u8; REF_BYTES]) -> Result<Option<Ref>, Error> {
    let u64_at = |at: usize| u64::from_le_bytes(slot[at..at + 8].try_into().unwrap());
    let u32_at = |at: usize| u32::from_le_bytes(slot[at..at + 4].try_into().unwrap());
    let (block, blocks) = (u32_at(16), u32_at(20));
    if blocks == 0 {
        return Ok(None);
    }
    let extent = ExtentId::new(u64_at(0), u64_at(8));
    ensure!(block < blocks, "{extent:?} names block {block} of {blocks}");
    Ok(Some(Ref { extent, block, blocks }))
}

/// The tree as the root keeps it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TreeRoot {
    /// The contents' bytes.
    pub size: u64,
    pub height: u32,
    pub top: Option<Ref>,
    /// The extents the last commit left less than half live, and the blocks each holds: the next
    /// commit writes their live blocks again, so that they die. At most [`SPARSE_KEPT`].
    pub sparse: Vec<(ExtentId, u32)>,
}

/// A block device's root.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Root {
    /// Where minting goes on from: every extent the block device wrote is below it.
    pub minted: u64,
    /// The extents to free once the root is durable: what its commit left unreferenced, and what
    /// earlier frees didn't free. At most [`DYING_KEPT`].
    pub dying: Vec<ExtentId>,
    /// An extent listing more of them, and how many ([`encode_list`]).
    pub dying_list: Option<(ExtentId, u64)>,
    pub tree: TreeRoot,
}

/// The bytes of an extent listing `count` extents, in blocks of `block_size`: none if past a
/// `usize`, as a count a root read back names may be.
pub(crate) fn list_len(count: u64, block_size: usize) -> Option<usize> {
    // Past a `usize`: none, which the caller refuses.
    let bytes = usize::try_from(count).ok()?.checked_mul(16)?;
    bytes.div_ceil(block_size).max(1).checked_mul(block_size)
}

/// The bytes of an extent listing `extents`, in blocks of `block_size`.
pub(crate) fn encode_list(extents: &[ExtentId], block_size: usize) -> Vec<u8> {
    let mut out: Vec<u8> = (extents.iter())
        .flat_map(|extent| [extent.cell.to_le_bytes(), extent.sequence.to_le_bytes()])
        .flatten()
        .collect();
    out.resize(list_len(extents.len() as u64, block_size).expect("a list in memory"), 0);
    out
}

/// The `count` extents `bytes`, an extent listing them, lists.
pub(crate) fn decode_list(bytes: &[u8], count: u64) -> Result<Vec<ExtentId>, Error> {
    let mut reader = Reader(bytes);
    (0..count).map(|_| reader.extent()).collect()
}

impl Root {
    pub fn encode(&self) -> Vec<u8> {
        assert!(self.dying.len() <= DYING_KEPT, "{} dying extents in a root", self.dying.len());
        let tree = &self.tree;
        assert!(tree.sparse.len() <= SPARSE_KEPT, "{} extents to compact", tree.sparse.len());
        let mut out = Vec::new();
        out.extend_from_slice(ROOT_MAGIC);
        out.extend_from_slice(&FORMAT.current.to_le_bytes());
        out.extend_from_slice(&self.minted.to_le_bytes());
        out.extend_from_slice(&(self.dying.len() as u64).to_le_bytes());
        for extent in &self.dying {
            out.extend_from_slice(&extent.cell.to_le_bytes());
            out.extend_from_slice(&extent.sequence.to_le_bytes());
        }
        let (list, count) = self.dying_list.unwrap_or((ExtentId::new(0, 0), 0));
        for word in [list.cell, list.sequence, count] {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out.extend_from_slice(&tree.size.to_le_bytes());
        out.extend_from_slice(&tree.height.to_le_bytes());
        let mut top = [0; REF_BYTES];
        encode_ref(&mut top, tree.top);
        out.extend_from_slice(&top);
        out.extend_from_slice(&(tree.sparse.len() as u64).to_le_bytes());
        for (extent, blocks) in &tree.sparse {
            out.extend_from_slice(&extent.cell.to_le_bytes());
            out.extend_from_slice(&extent.sequence.to_le_bytes());
            out.extend_from_slice(&blocks.to_le_bytes());
        }
        out
    }

    /// The root `bytes` hold: none if empty, as an extent device's root is until set. Untrusted
    /// (AGENTS.md, trust boundaries): the host's disk holds it, or another tenant's version a fork
    /// came from.
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    pub fn decode(bytes: &[u8]) -> Result<Option<Self>, Error> {
        if bytes.is_empty() {
            return Ok(None);
        }
        let mut reader = Reader(bytes);
        ensure!(
            reader.take(ROOT_MAGIC.len()).is_ok_and(|magic| magic == ROOT_MAGIC),
            "the device holds no block device"
        );
        FORMAT.check(reader.u32()?)?;
        let minted = reader.u64()?;
        let count = reader.u64()?;
        ensure!(count <= DYING_KEPT as u64, "a root naming {count} dying extents");
        let dying = (0..count).map(|_| reader.extent()).collect::<Result<_, Error>>()?;
        let (list, count) = (reader.extent()?, reader.u64()?);
        // More than a list holds is none, whose reading would ask for memory without bound
        // (board 8549).
        ensure!(count <= MAX_LISTED, "a root listing {count} dying extents, past {MAX_LISTED}");
        let dying_list = (count > 0).then_some((list, count));
        let size = reader.u64()?;
        let height = reader.u32()?;
        let top = decode_ref(reader.take(REF_BYTES)?.try_into()?)?;
        let count = reader.u64()?;
        ensure!(count <= SPARSE_KEPT as u64, "a root naming {count} extents to compact");
        let sparse = (0..count)
            .map(|_| Ok((reader.extent()?, reader.u32()?)))
            .collect::<Result<_, Error>>()?;
        ensure!(reader.0.is_empty(), "{} bytes past the root", reader.0.len());
        let tree = TreeRoot { size, height, top, sparse };
        Ok(Some(Self { minted, dying, dying_list, tree }))
    }
}

/// A cursor over a root's bytes, or a list's, read back from a device: untrusted.
struct Reader<'a>(&'a [u8]);

#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
impl<'a> Reader<'a> {
    fn extent(&mut self) -> Result<ExtentId, Error> {
        Ok(ExtentId::new(self.u64()?, self.u64()?))
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], Error> {
        let (taken, rest) = self.0.split_at_checked(len).context("the root ends early")?;
        self.0 = rest;
        Ok(taken)
    }

    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }

    fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(sequence: u64, block: u32) -> Ref {
        Ref { extent: ExtentId::new(3, sequence), block, blocks: 9 }
    }

    #[test]
    fn nodes_read_back_as_written() {
        let mut block = vec![0xaa; 4096];
        assert_eq!(fanout(4096), 170);
        let refs = [Some(at(1, 0)), None, Some(at(1 << 40, 8))];
        encode_node(&mut block, 2, 77, &refs);
        assert_eq!(decode_node(&block, 2, 77, 3).unwrap(), refs.to_vec());
        assert_eq!(decode_node(&block, 2, 77, 5).unwrap()[3..], [None, None]);
    }

    #[test]
    fn a_node_read_from_the_wrong_place_fails() {
        let mut block = vec![0; 512];
        encode_node(&mut block, 1, 4, &[Some(at(1, 0))]);
        let error = decode_node(&block, 1, 5, 1).unwrap_err();
        assert_eq!(error.to_string(), "node 5 of level 1 reads as node 4 of level 1");
        let error = decode_node(&block, 2, 4, 1).unwrap_err();
        assert_eq!(error.to_string(), "node 4 of level 2 reads as node 4 of level 1");
        let error = decode_node(&[0; 512], 1, 4, 1).unwrap_err();
        assert_eq!(error.to_string(), "node 4 of level 1 reads as no node (magic 0x0)");
    }

    /// Each object saying a version: the root, a node, an owners' list, each refused at a
    /// version outside the family's range, by name, as a build rolled back past a change is
    /// (doc-28). The version is the four bytes after a root's magic, after a node's, and an
    /// owners' list's first four.
    #[test]
    fn objects_of_a_version_not_read_are_refused() {
        let say = |bytes: &mut [u8], at: usize, version: u32| {
            bytes[at..at + 4].copy_from_slice(&version.to_le_bytes());
        };
        let refusal = |version: u32| {
            format!("block device version {version}: this build reads version 2 alone")
        };
        for version in [0, 1, 3, u32::MAX] {
            let mut root = Root::default().encode();
            say(&mut root, ROOT_MAGIC.len(), version);
            assert_eq!(format!("{:#}", Root::decode(&root).unwrap_err()), refusal(version));

            let mut node = vec![0; 512];
            encode_node(&mut node, 1, 4, &[Some(at(1, 0))]);
            say(&mut node, 512 - TRAILER_BYTES + 4, version);
            let error = decode_node(&node, 1, 4, 1).unwrap_err();
            assert_eq!(format!("{error:#}"), format!("node 4 of level 1: {}", refusal(version)));

            let mut owners = encode_owners(&[Owner::new(0, 7)], 512);
            say(&mut owners, 0, version);
            assert_eq!(format!("{:#}", decode_owners(&owners, 1).unwrap_err()), refusal(version));
        }
        let error = decode_owners(&encode_owners(&[Owner::new(0, 7)], 512), 2).unwrap_err();
        assert_eq!(error.to_string(), "a list of 1 owners where 2 were looked for");
        let error = decode_owners(&[2, 0, 0, 0, 1, 0], 1).unwrap_err();
        assert_eq!(error.to_string(), "the owners' list ends early");
    }

    #[test]
    fn owners_read_back_as_written() {
        let owners = [Owner::new(0, 7), Owner::new(3, 1 << 40), Owner::new(1, 0)];
        assert_eq!((owners[1].level(), owners[1].index()), (3, 1 << 40));
        let list = encode_owners(&owners, 512);
        assert_eq!(list.len(), 512);
        assert_eq!(decode_owners(&list, 3).unwrap(), owners);
        assert_eq!(owner_blocks(0, 512), 1);
        assert_eq!(owner_blocks(63, 512), 1);
        assert_eq!(owner_blocks(64, 512), 2);
        assert_eq!(owner_blocks(1024, 4096), 3);
    }

    #[test]
    fn roots_read_back_as_written() {
        let root = Root {
            minted: 1 << 33,
            dying: vec![ExtentId::new(1, 2), ExtentId::new(4, 5)],
            dying_list: Some((ExtentId::new(1, 9), 300)),
            tree: TreeRoot {
                size: 8192,
                height: 1,
                top: Some(at(7, 2)),
                sparse: vec![(ExtentId::new(3, 4), 12), (ExtentId::new(5, 6), 1)],
            },
        };
        assert_eq!(Root::decode(&root.encode()).unwrap(), Some(root));
        assert_eq!(Root::decode(&Root::default().encode()).unwrap(), Some(Root::default()));
        assert_eq!(Root::decode(&[]).unwrap(), None);
        let listed: Vec<_> = (0..300).map(|n| ExtentId::new(1, n)).collect();
        let list = encode_list(&listed, 4096);
        assert_eq!(list.len(), 8192);
        assert_eq!(list_len(300, 4096), Some(8192));
        assert_eq!(list_len(u64::MAX, 4096), None);
        assert_eq!(decode_list(&list, 300).unwrap(), listed);
        let error = Root::decode(b"something else").unwrap_err();
        assert_eq!(error.to_string(), "the device holds no block device");
        let error = Root::decode(b"FXSQLVOL\x04\0\0\0").unwrap_err();
        assert_eq!(error.to_string(), "the device holds no block device");
    }

    /// A root read back listing dying extents in an extent of their own is refused if it lists
    /// more than a list holds, before anything is read or allocated for them: a count of
    /// `u64::MAX` asked `list_len` for more than a `usize`, which wrapped, or panicked, and an
    /// allocation of gigabytes otherwise (board 8549). The edges: none, one, the most, one more,
    /// and the most a root's count holds.
    #[test]
    fn a_root_listing_more_than_a_list_holds_is_refused() {
        let root = |count: u64| {
            let dying_list = (count > 0).then_some((ExtentId::new(1, 9), count));
            Root { minted: 10, dying_list, ..Root::default() }.encode()
        };
        for count in [0, 1, MAX_LISTED - 1, MAX_LISTED] {
            let decoded = Root::decode(&root(count)).unwrap().unwrap();
            let listed = decoded.dying_list.map(|(_, count)| count).unwrap_or(0);
            assert_eq!(listed, count);
        }
        for count in [MAX_LISTED + 1, 1 << 40, u64::MAX - 1, u64::MAX] {
            let error = Root::decode(&root(count)).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("a root listing {count} dying extents, past {MAX_LISTED}")
            );
        }
        // And a root cut short anywhere past its magic is refused, never read past its end.
        let whole = root(3);
        for len in ROOT_MAGIC.len()..whole.len() {
            let error = Root::decode(&whole[..len]).unwrap_err();
            assert_eq!(error.to_string(), "the root ends early", "cut at {len}");
        }
    }

    /// The root at its largest, as many dying extents and extents to compact as it names, fits
    /// an extent device's root, the extent daemon's 4 KiB: TASK-551's root, which named every
    /// file, did not.
    #[test]
    fn the_largest_root_fits_a_devices_root() {
        let extents = |count: u64| (0..count).map(|n| ExtentId::new(u64::MAX, u64::MAX - n));
        let root = Root {
            minted: u64::MAX,
            dying: extents(DYING_KEPT as u64).collect(),
            dying_list: Some((ExtentId::new(u64::MAX, 1), u64::MAX)),
            tree: TreeRoot {
                size: u64::MAX,
                height: u32::MAX,
                top: Some(Ref { extent: ExtentId::new(u64::MAX, 1), block: 1, blocks: u32::MAX }),
                sparse: extents(SPARSE_KEPT as u64).map(|extent| (extent, u32::MAX)).collect(),
            },
        };
        // Which fits an extent device's root: block_device asserts it as it compiles.
        assert_eq!(root.encode().len(), MAX_ROOT_BYTES);
    }

    #[test]
    fn every_frozen_version_is_read() {
        formats::golden::check::<Objects>(env!("CARGO_MANIFEST_DIR"));
    }

    /// The family's objects, in blocks of [`CORPUS_BLOCK`].
    struct Objects;

    const CORPUS_BLOCK: usize = 512;

    /// The node of the corpus's: node 4 of level 1.
    const CORPUS_NODE: (u32, u64) = (1, 4);

    /// The extents the corpus's list of dying extents and its owners' list name.
    const CORPUS_LISTED: u32 = 3;

    impl formats::golden::Family for Objects {
        const FORMAT: formats::Format = FORMAT;

        fn write() -> Vec<(&'static str, Vec<u8>)> {
            let root = Root {
                minted: 1 << 33,
                dying: vec![ExtentId::new(0x2f, 2), ExtentId::new(0x2f, 5)],
                dying_list: Some((ExtentId::new(0x2f, 9), 300)),
                tree: TreeRoot {
                    size: 1 << 30,
                    height: 2,
                    top: Some(Ref { extent: ExtentId::new(0x2f, 7), block: 2, blocks: 9 }),
                    sparse: vec![(ExtentId::new(0x1e, 4), 12), (ExtentId::new(0x2f, 6), 1)],
                },
            };
            let made = Root { minted: 1, ..Root::default() };
            let mut node = vec![0xaa; CORPUS_BLOCK];
            let refs = [Some(at(1, 0)), None, Some(at(1 << 40, 8))];
            encode_node(&mut node, CORPUS_NODE.0, CORPUS_NODE.1, &refs);
            let owners = [Owner::new(0, 7), Owner::new(1, 4), Owner::new(3, 1 << 40)];
            assert_eq!(owners.len(), CORPUS_LISTED as usize);
            let listed: Vec<_> =
                (0..CORPUS_LISTED as u64).map(|n| ExtentId::new(0x2f, n)).collect();
            vec![
                ("root", root.encode()),
                ("root-made", made.encode()),
                ("node", node),
                ("owners", encode_owners(&owners, CORPUS_BLOCK)),
                ("dying-list", encode_list(&listed, CORPUS_BLOCK)),
            ]
        }

        fn written(version: u32, case: &str, bytes: &[u8]) -> Result<String, String> {
            // Version 2's decoders are this build's, its types the current ones.
            if version != 2 {
                return Err(format!("no decoder of version {version}"));
            }
            Self::read_as(case, bytes)
        }

        fn read(case: &str, bytes: &[u8]) -> Result<(), String> {
            Self::read_as(case, bytes).map(drop)
        }
    }

    impl Objects {
        /// `bytes` decoded as the object `case` names, by this build's decoders.
        fn read_as(case: &str, bytes: &[u8]) -> Result<String, String> {
            fn said<T: std::fmt::Debug>(decoded: Result<T, Error>) -> Result<String, String> {
                decoded.map(|value| format!("{value:#?}")).map_err(|err| format!("{err:#}"))
            }
            let (level, index) = CORPUS_NODE;
            match case {
                "root" | "root-made" => said(Root::decode(bytes)),
                "node" => said(decode_node(bytes, level, index, fanout(bytes.len()))),
                "owners" => said(decode_owners(bytes, CORPUS_LISTED)),
                "dying-list" => said(decode_list(bytes, CORPUS_LISTED as u64)),
                _ => Err(format!("no object {case}")),
            }
        }
    }
}
