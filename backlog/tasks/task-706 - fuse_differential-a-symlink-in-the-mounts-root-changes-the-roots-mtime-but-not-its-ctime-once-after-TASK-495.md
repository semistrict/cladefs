---
id: TASK-706
title: >-
  fuse_differential: a symlink in the mount's root changes the root's mtime but
  not its ctime, once, after TASK-495
status: To Do
assignee: []
created_date: '2026-11-20 20:33'
labels:
  - bug
  - fuse
  - flaky
dependencies: []
priority: low
ordinal: 1100000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen on ubu1 2026-11-20 in TASK-495's first verification run (its fix to stale cached attributes in, FUSE_NOTIFY_INVAL_INODE): 'call 52: Symlink { target: a, path: a }: of ., on the reference it mtime changed, ctime changed; on the mount, mtime changed, ctime stayed'. The calls before: 46 Unlink a, 49 Mknod c (socket), 50 and 51 Open c (ENXIO). Seen before TASK-495's fix too, as the same mixed result (mkdir after an fchmod of the root, 2026-11-16; symlink, 2026-11-18). A stale reply would leave both times; a mixed result means the root's cached ctime came from somewhere else (a kernel-local ctime, fuse_update_ctime_in_cache, or the server's reply). proptest could not shrink the case (281 lines): it depends on timing. Not seen again in 10 stress rounds of fuse_differential nor in TASK-495's reproduction test with root chmod and symlink checks (fuse_mount.rs changes_show_their_times_though_others_stat_the_nodes_meanwhile).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The mixed result is reproduced and its cause named, or shown gone in 50 stress rounds under full-suite load
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
