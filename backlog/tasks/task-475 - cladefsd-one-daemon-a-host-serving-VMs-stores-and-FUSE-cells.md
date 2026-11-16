---
id: TASK-475
title: 'cladefsd: one daemon a host, serving VMs'' stores and FUSE cells'
status: Done
assignee:
  - '@claude'
created_date: '2026-11-16 02:23'
updated_date: '2026-11-16 05:36'
labels:
  - host
  - fuse
dependencies: []
priority: high
ordinal: 877000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
A cell's volumes may be written by two processes of one host today: cladefs-extentd for VMs and `cladefs-mount --control` for FUSE, each taking the cell as one holder. Nothing can pause both and cut every volume at once, which whole-cell snapshots need (TASK-364.1, and the snapshot work below). The user chose (2026-11-15) one daemon, cladefsd, in place of both: one process holds every cell of the host, one store table, one bucket client, part cache, upload budget and drain. Also chosen: a full rename, binary cladefsd and env vars CLADEFSD_*; the extentd gRPC service stays, one service of cladefsd beside Mount.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The subtasks are done
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
One daemon a host: TASK-475.1 (rename), .2 (FUSE mount of cells in cladefsd), .3 (mounted volumes in the store table). Commit 1270ab9fe; full suites pass on macOS and Linux.
<!-- SECTION:FINAL_SUMMARY:END -->
