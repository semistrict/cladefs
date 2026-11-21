---
id: TASK-705
title: >-
  fuse_differential: unmounting at a case's end fails, fusermount3 -u 'Device or
  resource busy', now and then under load
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-20 20:33'
updated_date: '2026-11-20 22:50'
labels:
  - bug
  - fuse
  - flaky
dependencies: []
priority: low
ordinal: 1099000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen on ubu1 2026-11-20 (TASK-495's verification, fuse_differential under nextest --stress-count 10, fxjob 4 threads): 1 of 10 rounds failed 'unmounting: unmounting /mnt/fxdax-scratch/fxjob/<id>/.tmpXXXX: fusermount3 -u failed, exit status: 1: fusermount3: failed to unmount ...: Device or resource busy.' Also seen 2026-11-18 under the full suite's load (TASK-495's notes). The case's own fds are closed before the unmount (Case::run closes reference and subject), so something else holds the mount: a fd or cwd of the test's still open, a process scanning the scratch directory, or the kernel still releasing (async RELEASE). Find what holds it (fuser/lsof at the failure), then fix the test or the mount.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 What holds the mount at the failed unmount is named
- [ ] #2 fuse_differential unmounts cleanly in 50 stress rounds under load on ubu1
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Have a busy unmount say what holds it. 2. Find the holder (strace of the host's daemons). 3. Unmount the tests lazily, as production does.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Cause (2026-11-20, ubu1): a desktop session's trash monitor, gvfsd-trash (user ramon's GNOME session on ubu1), looks into every new mount: access(2) of its root, an inotify watch on it, .Trash and .Trash-1000 looked up; strace of the gvfs processes during the mount tests showed 76 such calls on the tests' mounts. A strict unmount coming during one of those calls fails EBUSY (fusermount3 -u 'Device or resource busy'), held by no process's fd or cwd: the call under way holds a path in the mount for its duration. Reproduced at once by new_roots_read_their_permissions_through_the_kernel (8 threads mounting and unmounting): every thread's unmount failed busy. Fix: the tests unmount lazily (unmount(true)), as cladefsd, cladefs-mount and Mount's Drop already do; join still waits until the kernel is done, and the store is checked after. A busy unmount's error now names what holds it as far as /proc shows (mount.rs holders()), saying a call under way may. Their mounts now at .gvfs too (TASK-306): gvfs leaves them alone. Verified: fuse_mount 12/12 and fuse_differential 30/30 stress rounds on ubu1; workspaces below.
<!-- SECTION:NOTES:END -->
