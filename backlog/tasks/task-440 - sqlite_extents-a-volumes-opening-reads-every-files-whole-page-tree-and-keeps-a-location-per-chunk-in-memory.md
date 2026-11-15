---
id: TASK-440
title: >-
  sqlite_extents: a volume's opening reads every file's whole page tree and
  keeps a location per chunk in memory
status: Done
assignee:
  - '@claude'
created_date: '2026-11-15 11:44'
updated_date: '2026-11-15 19:52'
labels:
  - sqlite
  - performance
dependencies: []
priority: medium
ordinal: 785000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Volume::open calls Pages::open for every file in the root (sqlite_extents/src/volume.rs), and Pages::open allocates a location for every chunk and node of the file (levels: Vec<Vec<Loc>>, 8 bytes each) and reads every node that isn't a hole before the first query (sqlite_extents/src/pages.rs). A 10 GiB database at 4 KiB blocks is 2.6M chunks: 21 MiB of locations and up to ~15.4K node reads (fanout 170) at opening; it grows with the database, not with what is read. The eager walk also builds each extent's live count, which compaction uses, so a lazy tree needs those counts kept elsewhere. Found 2026-11-15 when the same tree was considered for a cell's memory volume, whose 64 GiB images make it 128 MiB and ~99K node reads.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Opening a volume reads its root and no file's nodes; a file's nodes are read as reads and commits reach them, shown by a test counting device reads at opening and at a first read
- [x] #2 Memory per open file grows with the chunks and nodes touched, not with the file's size, shown by a test over a sparse file of 64 GiB
- [x] #3 Compaction keeps working without the whole tree in memory (live counts kept on the device), shown by a test that compacts after a reopening
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Decided with the user 2026-11-15: lazy tree; per-extent owner lists persisted (not a counts tree); counts derived at commit.
1. format: ROOT_VERSION 3. An extent written by a commit holds its blocks, then its owner list: each block's owner (level<<58|index, u64 LE), padded to whole blocks (owner_blocks(n) = ceil(8n/block)). FileRoot gains sparse: the extents a commit left under half live, to compact at the next commit, at most SPARSE_KEPT (16) a file.
2. pages: no per-chunk locations, no slots. Committed root (size, height, top, sparse) plus a cache of the nodes read or written (children refs by (level, index)). Lookups descend from the top, reading nodes on demand. read/write/truncate/fetch/runs async where they look up.
3. Commit plan: load phase (async): compaction chunks and nodes from the sparse extents' owner lists, each owner live iff the committed tree still names that block; the committed nodes under every node rebuilt; the old refs of everything placed, cut (cut..old_n) or past the new shape (released). Then for each extent released blocks touch: its owner list, and its live blocks after the commit (owners the tree still names and the commit neither places nor releases): 0 -> dead (freed as today, through the root's dying), under half -> sparse. Sync phase: placement and encoding as today. Plan::write writes each extent's blocks and owner list.
4. apply: the root and the cache's rewritten nodes; nodes past the new shape dropped.
5. volume: recover reads no file's nodes; extents()/delete walk a file's tree (inherent: delete frees all of it); open_file maps nothing; fetch maps the chunks it asks for, if not mapped yet (stale ones as before, in batches).
6. Tests: pages model tests with reopenings and frees checked against the device; device reads counted at opening (only the root) and at a first read (one path); a sparse 64 GiB file's cache holding only touched nodes; compaction after a reopening frees the sparse extent; crash and sqlite suites as they are. macOS scripts/test.sh part for sqlite_extents, then cladefs's (extentd uses volumes), once.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented 2026-11-15 (branch t440-lazy-pages). format: ROOT_VERSION 3; extents end with their blocks' owners (Owner, 8 bytes a block, padded to blocks); FileRoot.sparse (at most 16). pages: Tree (root's top, level counts, nodes read or written) looked up from the top on demand, nodes read a level at a time, 32 in flight; plan reads the nodes it rewrites and those of what it leaves, the owners of the extents it leaves blocks of, and counts their survivors by looking each owner up: none -> dead, under half -> sparse; the next commit rewrites a sparse extent's live blocks. volume: recover reads the root alone; a file's region maps what reads and fetches reach (a bitmap per region chunk), stale chunks remapped in batches as before; extents()/delete read a file's tree whole. Found on the way: narrowing the rebuild over the cut to chunks the last commit had (the eager tree rewrote hole nodes up to the new end, 88 s for a 64 GiB file) dropped the nodes above an old top when the tree grew two levels in one commit; fixed (above(old_height + 1, 0)) and pinned by a_tree_grown_two_levels_keeps_its_old_top; random model test now 100 seeds (400 run once, clean). Validation: macOS CLADEFS_TEST_PARTS='clippy workspace' scripts/test.sh: clippy clean, 2503 passed; ubu1 (fxjob) workspace part: 2803 passed; ubu1 sqlite_extents + cladefs sqlite tests 33 passed; scripts/upstream-changes.py --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
A SQLite volume's opening reads its root alone; a file's page tree is read as lookups reach it and kept, so memory grows with what is touched (a 64 GiB sparse file holds only its paths' nodes). Space without the whole tree: each extent ends with its blocks' owners, and a commit counts the survivors of the extents it leaves blocks of by looking their owners up, freeing the dead and naming the under-half-live in the root for the next commit to compact. Windows map what reads and fetches reach instead of the whole file at opening. Verified by new tests (reads counted at a volume's opening and a first read, a sparse 64 GiB file, compaction after a reopening, a tree grown two levels) and the full suites on macOS (2503) and Linux on ubu1 (2803).
<!-- SECTION:FINAL_SUMMARY:END -->
