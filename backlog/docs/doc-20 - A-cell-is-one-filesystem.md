---
id: doc-20
title: A cell is one filesystem
type: specification
created_date: '2026-11-18 00:08'
updated_date: '2026-11-18 00:09'
---
# A cell is one filesystem

Design, 2026-11-17. The user's decision, 2026-11-17: there are no vdisks. A cell is one
filesystem, and what a vdisk was is a cell. Supersedes doc-19 (one log per cell, several vdisks as
slots of it) and doc-18's whole-cell section; replaces TASK-364's cell of several vdisks. The
branch t3/one-log-per-cell, which built doc-19, is kept unmerged, as a reference.

## The model

- **A cell** is one filesystem (Fxfs, a SQLite file set, or a VM's memory image: its **kind**),
  with one writer at a time, its log (open segment, write-ahead log, segments, checkpoints), its
  head, its key (KEK, DEKs), and its versions. It is the unit of concurrency, placement, fencing,
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

## Why

A cell of several vdisks needed either one log per vdisk, so that a version of the whole cell took
N pauses, N commits, N uploads, N head swaps and an object naming them (doc-18, cell cuts), or one
log for all of them (doc-19), which moved the "several vdisks" into the store's core: per-vdisk
uncommitted changes, space and roots, slots, a rule for closing segments other vdisks write in,
vdisk handles to keep vdisks apart, and slots packed into extent IDs. The branch that built doc-19
grew the code by about 3,700 lines. A paused VM needs none of it: a VM snapshot's versions agree
because nothing writes while they are made.

## Words

Retired, in code, docs and the glossary, by the end of this work:

| Retired | Say |
|---|---|
| vdisk | cell |
| branch | cell (the extent ID's first half, too: the cell that wrote it) |
| point | version (a shared point is a shared version) |
| slot, namespace | none: a cell has one filesystem |
| cut | make a version |

Kept: the extent ID's first u64, which names the cell that wrote the extent: attached bases, Fxfs
volumes over a read-only base of another cell, need it (doc-10).

## Order of work

1. Docs, tasks and glossary (this doc).
2. A cell has one filesystem: the cell object's vdisk map, add/remove vdisk, `cell_cut` and whole-
   cell cuts go; a cell has a kind. A VM's disk and memory are cells of their own; the daemon, its
   API, the FUSE mount and the Go bindings follow.
3. Versions replace points: forks only from versions; a shared point is a shared version.
4. The cell object and the head merge, and the types `Cell` and `Branch` become one.
5. The rename: branch, vdisk and point leave the code (types, fields, protos, the extent ID's
   `branch`, Fxfs's uses of it, the guest kernel's `vext_id`).
