---
id: TASK-476
title: >-
  Fast, cheap snapshots of volumes and cells: local-first points in the source
  cell
status: To Do
assignee: []
created_date: '2026-11-16 02:25'
updated_date: '2026-11-16 03:28'
labels:
  - snapshot
  - bucket
  - host
dependencies:
  - TASK-475
priority: high
ordinal: 881000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The user wants extremely fast and cheap snapshots at volume and cell level (2026-11-15). Today a VM snapshot pauses the VM while the store's whole upload backlog uploads (fork_at_rest -> Store::commit_fork -> Remote::forked waits for the fork epoch's acknowledgement, in order, up to the 1 GiB lag), then makes 20 requests in 18 serial round trips and writes a whole-state checkpoint twice (TASK-452), because each snapshot is a fork into a new cell. Decided with the user that day: (1) local-first: a snapshot returns once its fork commit is durable on local disk; the point is pending until acknowledged, its record made by the uploader then; same-host restores work at once, other hosts wait for durable; (2) points live in the source's cell, as versions, not a new cell: no cross-cell checkpoint or reference at snapshot time, the first restore into another cell pays the fork-point checkpoint once; (3) whole-cell cuts in one process, cladefsd (TASK-475), the holder taking every volume's commit lock in order, one object naming each volume's (branch, at) as the atomic commit; (4) scale: frequent snapshots, few kept (a rolling window), so taking and deleting a point must be cheap and GC must reclaim promptly. Related: TASK-452, TASK-364.1, TASK-242, TASK-456.4.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Design written into a backlog doc and doc-7, checked by the bucket simulation and specs/bucket before coding
- [ ] #2 A volume snapshot pauses its VM for no bucket round trip: its pause is the freeze and one local fsync, measured on ubu1
- [ ] #3 A snapshot makes O(1) bucket requests and bytes not growing with the extents (bucket_lifecycle_by_phase)
- [ ] #4 A whole-cell snapshot cuts every volume at one moment, shown by a test committing to two volumes concurrently
- [ ] #5 The daemon lists the volumes it holds of a cell, VM-served and mounted alike, each with a handle that pauses its commits (moved from TASK-475.3)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
