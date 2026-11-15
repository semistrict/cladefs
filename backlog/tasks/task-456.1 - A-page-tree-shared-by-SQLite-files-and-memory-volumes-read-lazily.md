---
id: TASK-456.1
title: 'A page tree shared by SQLite files and memory volumes, read lazily'
status: To Do
assignee: []
created_date: '2026-11-15 13:33'
labels:
  - memory
  - sqlite
dependencies:
  - TASK-440
parent_task_id: TASK-456
priority: high
ordinal: 862000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
sqlite_extents/src/pages.rs keeps a file's bytes as 4 KiB chunks over write-once extents in a radix tree (fanout 170), holes reading as zeros, copy-on-write commits, compaction of half-dead extents. A memory volume needs the same tree for images of up to 64 GiB (16.7M chunks), which Pages::open's eager walk and per-chunk locations can't serve (TASK-440). One lazy tree for both.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The page tree is a module both sqlite_extents and memory volumes use, its format unchanged for SQLite files or changed with a format bump (no migration, AGENTS.md)
- [ ] #2 TASK-440's criteria hold for it: no node read at opening, memory growing with what is touched, compaction from live counts kept on the device
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
