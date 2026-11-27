---
id: doc-20
title: A cell has one kind
type: specification
created_date: '2026-11-18 00:08'
updated_date: '2026-11-18 03:46'
---
# A cell has one kind

Design, 2026-11-17 (TASK-508). The user's decision, 2026-11-17: a cell holds one thing, of one
kind, and nothing inside it is named apart. Supersedes doc-19 and doc-18's whole-cell section.

## The model

- **A cell** has one kind (Fxfs, SQLite, or a VM's memory image), a root and a capacity, one
  writer at a time, its branch (open segment, write-ahead log, segments, checkpoints), its head,
  its key (KEK, DEK), and its versions. It is the unit of concurrency, placement, fencing,
  encryption and collection. A host holds it whole.
- **A version** is a read-only state of a cell in its past, kept inside the cell. Only the cell's
  writer makes or drops one. Nothing else writes it.
- **Making a version is local:** one commit in the cell's local log that closes its segment.
  Nothing on that path touches the bucket. Uploading the segment and naming the version in the head
  happen later, in the background, as every upload does (doc-18, "local at once, uploaded later").
- **A fork** makes a new cell from a version of another, its **parent**; the new cell is the
  **child**. Forks are made only from versions. A fork on the version's host is local too; a fork
  taken on another host needs the version uploaded first.
- **Dropping a version** that has children keeps its data until those children go, as a parent's
  extents are kept for a child today.
- **A VM** is several cells, typically its disk (Fxfs) and its memory image. A **VM snapshot** is a
  version of each of the VM's cells plus the record naming them, and is host-local: the VM's cells
  are always held by the VM's host, so it pauses the VM, makes a local version in each cell, then
  writes the record naming them, last. The VM is paused, so the versions agree with no
  coordination; one cut short writes no record, and its versions are dropped. No bucket round trip
  and no cross-host coordination.
- **A copy** is a cell holding a version, which nothing writes: forks of the version are made
  from it, on the version's host or, made there first (`CreateCell` with the version), on any
  other. A VM snapshot's restores fork its copies.
- **An origin** is where a child was forked from: its parent and the segment the version closed.
  The collector keeps an origin's state while children in other cells live on it.
- **A cell's ID is unique in its bucket**, given by the bucket's allocator, and is its branch's ID
  too: no caller names a new cell (`CreateCell` with a cell of 0, the FUSE mount's `Attach` of a
  cell of 0).
- **An extent ID** names the cell that wrote the extent and the sequence number it gave it: a fork
  reads extents of the cells it was forked from, which only the first half tells apart. The first
  half is the cell's branch's ID, so unique in the bucket.

## Why

A cell holding several disks needed either a log for each, so that a version of the whole cell
took N pauses, N commits, N uploads, N head swaps and an object naming them (doc-18), or one log
for all of them (doc-19), which moved the several disks into the cell's core: per-disk
uncommitted changes, space and roots, numbers packed into extent IDs, a rule for closing segments
others still write in, and handles to keep them apart. The branch that built doc-19 grew the code
by about 3,700 lines. A paused VM needs none of it: a VM snapshot's versions agree because nothing
writes while they are made.

## Order of work

1. Docs, tasks and glossary (this doc).
2. A cell has one kind: the cell object names its one branch and kind, `cell_cut` goes, and the
   daemon, its API, the FUSE mount and the Go bindings make and open cells (TASK-508.1).
3. Versions are the only named past states: forks only from versions, through copies; points go
   from the code, the specs, the daemon's API and Firecracker's snapshot fields (TASK-508.2).
4. A cell's ID is unique in its bucket, its branch's ID (TASK-508.2, at the user's ask).
5. One concept, one type: the cell object and the head merge, and the types that name a cell today
   (`Cell`, `Branch`) become one, `Cell`: one kind, root and capacity, one branch (TASK-508.3,
   later).
6. The code, its protos and docs say the glossary's words throughout (TASK-508.4, later).
