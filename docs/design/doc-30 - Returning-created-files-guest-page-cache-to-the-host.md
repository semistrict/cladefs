---
id: doc-30
title: "Returning created files' guest page cache to the host"
type: specification
created_date: '2026-11-26 00:00'
updated_date: '2026-11-26 00:00'
---
# Returning created files' guest page cache to the host

Design, 2026-11-26 (board 13454). Revised after review (hotbed review 147) and measurement
([ubu1-guest-page-cache-2026-11-26](../measurements/ubu1-guest-page-cache-2026-11-26.md)).

## Goal

A build job in a DAX-mounted cladefs guest does not hold gigabytes of clean guest page cache for
the files it created: those pages go back to the host, and later reads of those files go through
the host's shared DAX window instead of a second copy in the guest.

## What the measurement found

On ubu1, an 18.5 GiB guest building `cladefs` into a new target directory:

- The guest's file cache, 7.9 GiB against 0.2 GiB anon, is the build's outputs, clean. hotbed's
  dirty limits (d0e78922) leave nothing dirty. Nothing comes back on its own.
- Dropping it reaches the host within seconds through free page reporting at its default order 9:
  6.0 GiB from fadvise on the outputs alone, 4.4 GiB from `drop_caches 3`, 0.9 GiB more from
  compaction. About 2.2 GiB of free guest memory stays in blocks of order 0 to 8.
- The next build step was no slower after a drop.
- Most outputs never read in again as DAX (1 of 7041): their inodes are never evicted, because the
  kernel's inode LRU walk livelocks on an unused DAX inode with one DAX entry at index 0
  (board 13582). Every small file a build reads through the window is such an inode.

## Design

### 1. The inode LRU evicts past DAX inodes (board 13582)

`mapping_shrinkable()` takes a mapping whose one entry, at index 0, is a value entry for one
holding a shadow entry, which invalidating the mapping clears. A DAX entry it leaves, so
`inode_lru_isolate()` retried such an inode for good, evicting none behind it. Linux fixed it
upstream (8ee09f597763, "fs: rotate inodes whose page cache could not be dropped", for 7.4, which
names this case), a regression from 2a0629834cd8: the inode it cannot empty rotates to the list's
tail, and the walk goes on past it. Stable has it from 6.18.64; Amazon Linux's newest release is
6.18.62. So the guest kernel moves to 6.18.62 and carries the fix, rebased onto it, and its
companion as guest/linux/patches 0014-0015, until Amazon ships 6.18.64. 6.18.64's other
prerequisite, 972b9dd4e418, is left out: without ext4's 77ff1ff2f3c5 it lets an ext4 filesystem
with no journal lose a dirty metadata buffer as its inode is evicted (hotbed review 155).
Created files then leave memory when the kernel evicts their inodes, and read in again as DAX
(doc-13), with no mechanism of cladefs's own.

### 2. When the guest gives its cache back: hotbed's guestd

The policy lives in hotbed's guestd, which knows when a command is a build and when the host is
short (hotbed builds 261, 266: `sync`, `drop_caches 3`, compaction on reclaim, and a proactive
reclaim on host pressure). With §1, reclaiming a command's memory cgroup (`memory.reclaim`, which
shrinks its slab too) drops its clean pages and evicts its unused inodes, so the outputs it built
read in again as DAX: a gentler trigger than `drop_caches 3`, which drops every dentry and Fxfs's
cached records too.

### 3. Free page reporting's order: measure first

Order 9 left 2.2 GiB of a drained 18.5 GiB guest's free memory unreported. ubu1's host THP is
`madvise`-only and hotbed's VMs ask for no huge pages, so a lower `page_reporting_order` splits no
host THP there; on VMs restored from a memory cell each reported range is one `reclaim` call, so a
lower order multiplies them. An A/B of order 9 against 7 on ubu1 decides it.

## Rejected

| Option | Why not |
|---|---|
| Evicting each created inode from the DAX worker after its seal, with a cooldown and a cap (the first draft's §1) | The kernel evicts them once §1 stops the livelock; the cooldown and cap contradicted each other, and `drop_inode` with a flag risked losing times (review 147). |
| `d_mark_dontcache` on still-open files at a seal (the first draft's §2) | Makes `close()` a synchronous write, and flips databases loaded across a seal to copy on write. |
| Drop-behind at writeback | Re-reads of a live page-cache inode fill fresh folios that never pass writeback again. |
| DAX for created files (undoing doc-13 Option 5) | Private pages and range doubling on every write, no temp-file elision, no staging-extent slices. |
| Dirty thresholds in cladefs | hotbed's guestd sets them at boot (d0e78922); nothing in the cache was dirty. |

## Verification

- Guest check `cow_created_behind_a_dax_inode` (guest/tests): an unused DAX inode with one DAX
  entry at index 0 ahead of eight created, sealed files on the inode list; each must read in again
  as DAX after `drop_caches`. Red on 6.18.54, green on 6.18.62 with patches 0014-0015.
- scripts/measure-guest-page-cache.sh again on the patched kernel: outputs read in as DAX after a
  drop, and a rebuild after it leaves the guest's file cache near empty.
