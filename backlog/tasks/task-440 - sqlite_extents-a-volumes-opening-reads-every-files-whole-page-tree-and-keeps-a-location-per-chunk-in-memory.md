---
id: TASK-440
title: >-
  sqlite_extents: a volume's opening reads every file's whole page tree and
  keeps a location per chunk in memory
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 11:44'
updated_date: '2026-11-15 19:04'
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
- [ ] #1 Opening a volume reads its root and no file's nodes; a file's nodes are read as reads and commits reach them, shown by a test counting device reads at opening and at a first read
- [ ] #2 Memory per open file grows with the chunks and nodes touched, not with the file's size, shown by a test over a sparse file of 64 GiB
- [ ] #3 Compaction keeps working without the whole tree in memory (live counts kept on the device), shown by a test that compacts after a reopening
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
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
