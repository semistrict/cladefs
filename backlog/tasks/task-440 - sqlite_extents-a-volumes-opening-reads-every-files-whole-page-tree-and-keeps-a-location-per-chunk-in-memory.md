---
id: TASK-440
title: >-
  sqlite_extents: a volume's opening reads every file's whole page tree and
  keeps a location per chunk in memory
status: To Do
assignee: []
created_date: '2026-11-15 11:44'
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
