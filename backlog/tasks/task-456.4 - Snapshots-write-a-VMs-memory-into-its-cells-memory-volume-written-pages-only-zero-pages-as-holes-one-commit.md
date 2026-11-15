---
id: TASK-456.4
title: >-
  Snapshots write a VM's memory into its cell's memory volume: written pages
  only, zero pages as holes, one commit
status: To Do
assignee: []
created_date: '2026-11-15 13:33'
labels:
  - memory
  - snapshot
  - firecracker
dependencies:
  - TASK-456.3
  - TASK-364.1
parent_task_id: TASK-456
priority: high
ordinal: 865000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Firecracker's /snapshot/create writes mem_file_path, all of guest memory for a full snapshot. With guest memory a copy-on-write window over a memory volume, the pages a snapshot needs are the private ones; for a VM booted cold (anonymous memory) they are the resident ones. The snapshot's disk point and memory belong in one cell (TASK-364.1, whole-cell forks).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A snapshot of a restored VM writes its private pages into a fork of its parent memory volume, all-zero ones as holes, in one commit, and Firecracker writes no memory file, shown by a guest test counting the bytes stored
- [ ] #2 A snapshot of a VM booted cold stores its resident non-zero pages and nothing of memory it never touched, shown by a guest test of a 4 GiB VM
- [ ] #3 The snapshot's disk point and memory volume are in one cell, restorable on another host from the bucket, shown by a test
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
