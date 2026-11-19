---
id: TASK-655
title: >-
  virtio_extent windows_are_where_snapshots_had_them failed once in a Linux
  workspace run: its restore's open got ENOENT
status: To Do
assignee: []
created_date: '2026-11-19 10:27'
labels:
  - virtio_extent
  - flaky
dependencies: []
priority: low
type: bug
ordinal: 1036000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
On ubu1 (fxjob, scripts/test.sh workspace part, --test-threads 4) on the batch-2 commit 808ed60ca (wire version 11, TASK-550..612), virtio_extent::daemon windows_are_where_snapshots_had_them failed once: Device::restore of the snapshot's state panicked at crates/virtio_extent/tests/daemon.rs:729 with Store("…/restored", opening the extent store at …/restored: ENOENT). The test takes a snapshot (a version made on the device's connection), drops the device, then restores at once, which forks the version. It passed 40 times of 40 alone and 15 rounds of 15 of the whole virtio_extent daemon binary at --test-threads 8 on the same commit, and passes on main's CI. A guess, unchecked: the restore reads the cell's head before the store the device closed has uploaded the version, which its closing does after the device is gone.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The cause of the ENOENT is found and the restore waits for, or fails clearly without, a version not uploaded yet
- [ ] #2 windows_are_where_snapshots_had_them passes 200 times of 200 under a loaded workspace run
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
