---
id: TASK-726
title: >-
  Forks through a version fail with EIO when TMPDIR is on another filesystem
  than the part cache: version_dir is in temp_dir
status: To Do
assignee: []
created_date: '2026-11-21 08:05'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 1131000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
host_cell::version_dir keeps a store at a version, which forks are made from, in std::env::temp_dir()/cladefs-versions-PID. LocalCell::open refuses a store on another filesystem than the host's part cache (TASK-359, cell.rs: 'the store is on another filesystem than the host's cache'). So with TMPDIR (often /tmp on tmpfs) on another filesystem than CLADEFSD_PART_CACHE, every fork of a live store fails: the fxfs-guest-pgbench-dax case's fork, run with its results on a RAM-backed ext4 and TMPDIR on ubu1's scratch disk, failed 'forking the store at .../base to .../fork: EIO', the daemon logging the refusal (TASK-707, ubu1:/mnt/fxdax-scratch/pgb-ram/fxfs-guest-pgbench-dax/old-1, de9638df; main's version_dir is the same). Tests never see it: their TMPDIR and stores share a filesystem.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A store at a version, made for forks, is on the part cache's filesystem (beside the cache, or in the forked store's root), whatever TMPDIR is
- [ ] #2 A test forks a live store with TMPDIR on another filesystem than the part cache, and the fork succeeds
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 scripts/upstream-changes.py --check passes
- [ ] #2 Deviations from upstream documented where they occur
<!-- DOD:END -->
