---
id: TASK-475.3
title: >-
  One store table: FUSE volumes held in the daemon's table, a cell's volumes
  found by cell
status: Done
assignee:
  - '@claude'
created_date: '2026-11-16 02:25'
updated_date: '2026-11-16 05:36'
labels:
  - host
  - fuse
dependencies:
  - TASK-475.2
parent_task_id: TASK-475
priority: high
ordinal: 880000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Third step of TASK-475. In one process, a store a VM writes and a store the mount has attached must still exclude each other, and control calls that change a store (fork, delete, resize) must see a mounted one as held, as they see a VM's. A whole-cell cut (TASK-364.1) needs every volume of a cell the host holds, whichever side serves it, found from the cell.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A volume attached to the mount is held in the daemon's store table: a VM opening it, or a control call that would change it, is refused (EBUSY) as for a VM-held store, shown by a test
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
AC #2 (a cell's volumes listed, each with a handle that pauses its commits) moved to TASK-476 (2026-11-15): only the whole-cell cut needs it, and the cut's design decides the handle's shape. Implemented AC #1: store_table Entry::Mounted / Claim::Mount / Refused::Now (EBUSY at once, no wait); extentd::stores::hold_mounted; fuse::control holds each attached volume's store from before it opens until detached. Unit tests stores_mounted_are_the_mounts_alone, the_mount_attaches_stores_nothing_else_has pass on macOS, Shuttle store_table tests pass (11).

DoD #1: scripts/test.sh clippy+workspace on commit 1270ab9fe: macOS 2517/2517, Linux (ubu1, fxjob) 2827/2827; CI's clippy rules clean on both.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Mounted volumes held in the daemon's store table (Entry::Mounted): VMs and control calls get EBUSY at once. Cell volume listing with a commit-pause handle moved to TASK-476. Verified: store_table unit and Shuttle tests, a_volume_mounted_is_the_mounts_alone on Linux, full suites.
<!-- SECTION:FINAL_SUMMARY:END -->
