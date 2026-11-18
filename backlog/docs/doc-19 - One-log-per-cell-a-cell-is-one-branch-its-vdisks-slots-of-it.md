---
id: doc-19
title: 'One log per cell: a cell is one branch, its vdisks slots of it'
type: specification
created_date: '2026-11-17 02:45'
updated_date: '2026-11-18 00:24'
---
> **Superseded** (2026-11-17) by doc-20, "A cell is one filesystem". Built on branch t3/one-log-per-cell, kept unmerged as a reference.

# One log per cell: a cell is one branch, its vdisks slots of it

Design, 2026-11-17 (TASK-506). The user's decisions, 2026-11-17: a cell is the unit the one
writer owns, with every vdisk in it and every version of it; a **version** is always a whole
cell's, never a vdisk's; the word "cut" is retired (CONTEXT.md). Supersedes TASK-364's "one log per
vdisk" (doc-7, Cells) and doc-18's whole-cell section.

## Why

A cell is placed whole, held by one writer at one epoch, and its vdisks' heads are all taken with
that epoch (`extent_bucket::cell`): fencing is the cell's already, spread over N heads. With one
log per vdisk a version of the whole cell needs every vdisk paused in order, a segment-closing
commit on each, N uploads and N head swaps, and an object naming them all once every member is
uploaded (doc-18, Whole-cell cuts). With one log, every position in it is one state of every vdisk:
a version is one commit, one segment, named in one head. Fxfs is built so (one journal, many
volumes), and so is `extent_files::FileSet` (many files, each committed alone, one root).

TASK-364 chose one log per cell first, and turned from it the same day because the store assumed
one client per branch: a commit seals every open extent and takes every free; one sequence of
extent IDs; orphan sweeps by that sequence; frees and capacity per branch. Each is answered below.

## The model

- **A cell is one branch:** one log (open segment, write-ahead log, segments, checkpoints), one
  head, one writer, one record, one DEK. Its store is the cell's; the daemon opens it once.
- **A vdisk is a slot of its cell's branch**, `0..VDISK_SLOTS` (2^16): a name, a kind (Fxfs,
  SQLite, memory), a capacity, a root, its next sequence number and its spent sequence numbers.
- **A vdisk's extents are named by its namespace**, `branch.id + slot`, the `branch` field of the
  `ExtentId` its guest mints. Slot 0's namespace is the branch's ID, as every extent's is today.
  The object ID allocator gives branch IDs aligned to `VDISK_SLOTS`, so namespaces never meet;
  a store refuses a vdisk past slot 0 on a branch whose ID is not aligned (tests name small IDs).
  The guest sees nothing new: its `branch` is its vdisk's namespace, a different one after a fork
  (the child's branch ID differs), as today. Its orphan sweep covers its namespace alone, and its
  next sequence number and floor are its vdisk's.
- **A commit is one vdisk's:** it seals the open extents of that vdisk's namespace, records that
  vdisk's frees, root and capacity, and nothing of the others'. The commits of all the vdisks are
  in one order, the log's; a commit's number is the store's, as now.
- **Making and dropping a vdisk are commits** of the cell, recorded in the log with the rest.
  Slots are never reused within a branch: a dropped vdisk's slot stays spent, so that no two
  vdisks of a branch's history ever share a namespace.
- **A version is a state of the cell**: the commit that closes the cell's segment, its store
  quiescent (no extent open in any vdisk), named by the head once uploaded (doc-18's mechanism,
  unchanged: `Store::commit_version`, `Head::versions`). `cell_cut` goes: a version is always the
  whole cell's.
- **A fork is of the whole cell**: the child is a new cell, whose branch has the parent's vdisks
  at the same slots, so new namespaces at once. A point is a whole cell. Forking a single vdisk out
  of a cell of several is not offered (yet): fork the cell and drop the vdisks the child doesn't
  want. Its parent's fork point then keeps the whole cell's state until the child goes, which costs
  space, never correctness.

## Formats

No migration (AGENTS.md): the formats change and refuse the old.

- **Footers** (`extent_frame::segment::Contents`): `root`, `next_sequence` and `capacity` go;
  `vdisks: BTreeMap<u16, Option<VdiskState>>` names each vdisk the segment's commits changed, as
  the last of them left it (`None`: dropped). `VdiskState { name, kind, root, next_sequence,
  capacity }`. `own` names each extent of its own by its full ID. A segment's commits fold per
  vdisk, the last commit of each vdisk winning, as the root did.
- **An own extent's key** names its branch, not its namespace, so that `keys::branch_of` and the
  collector find the branch's DEK and owner from the key alone.
- **The branch's state** (`extent_bucket::recovery::State`, checkpoints): `vdisks: BTreeMap<u16,
  VdiskState>` in place of `root`, `next_sequence`, `capacity`; `spent` per slot.
- **The store's description** names no vdisk: a store is its cell's branch. Its vdisks are in its
  log.
- **The cell's object** names its branch, not a map of vdisks: the vdisks are in the log, carried
  by versions and forks as data. Taking the cell takes the one head.
- Bucket `FORMAT` 15, store format 14.

## The store

- **Uncommitted changes are per vdisk:** open extents and changes kept by slot. A write goes to its
  extent's namespace's vdisk; a free names the vdisk freeing it (an inherited extent's namespace is
  another branch's). A commit of vdisk `v` takes `v`'s alone.
- **The write gate stays the store's:** a commit waits for every write in progress, of any vdisk,
  as now. Simpler, and only as slow as one write.
- **Segments close only when no extent open has pieces in them**, of any vdisk: a vdisk's commit
  closing a segment another vdisk still writes into leaves it open until that vdisk commits. A
  segment filled stops taking writes as now.
- **Space:** each vdisk has its capacity, which its guest is told; holding is counted per vdisk
  (its namespace's own extents). The store's room is the sum.
- **Followers** of a vdisk follow the store's commits, of every vdisk: a batch of commits that
  changed nothing of theirs applies nothing. Simplest; filtered wakeups are a later optimisation.
- **One vdisk's panic halts the store**, and so the cell: the cell is the unit that fails.

## The daemon

- **A cell's store is opened once**; each connection holds one vdisk of it, a slot, and the
  device's `branch` is that vdisk's namespace. A VM's vdisks (its disk, its memory) and a mount's
  are vdisks of one store.
- **A VM snapshot** takes a version of its cell, its vdisks quiescent.

## What goes

Per-vdisk heads, records, DEKs and uploaders; the cell object's vdisk map; cell cuts
(`cell_cut`, `SnapshotCell`'s coordination, doc-18's Whole-cell cuts); TASK-364.1's
pause-every-vdisk fork (a fork of the cell's branch is the whole-cell fork).

## Order of work

1. extent_frame, extent_bucket's state and checkpoints, extent_store: a store of several vdisks,
   commits per vdisk; cladefs on slot 0 everywhere, so that every test passes unchanged.
2. Branch IDs aligned by the allocator; own extents keyed by branch.
3. cladefs: a cell's store shared by its vdisks' connections; the FUSE mount's vdisks in one store.
4. Cells in the bucket: one branch each; `cell::Object::vdisks` gone; vdisk names in the log.
5. Versions the cell's: `cell_cut` gone, `SnapshotCell` a version of the cell's branch.
6. doc-7, doc-14, doc-18, CONTEXT.md.
