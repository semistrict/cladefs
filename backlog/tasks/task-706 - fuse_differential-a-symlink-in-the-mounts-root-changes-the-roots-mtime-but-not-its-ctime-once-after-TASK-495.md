---
id: TASK-706
title: >-
  fuse_differential: a symlink in the mount's root changes the root's mtime but
  not its ctime, once, after TASK-495
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

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Have the time failure say what the kernel showed and what the filesystem has (statx AT_STATX_FORCE_SYNC). 2. Check the server alone. 3. Name the cause and keep host processes off the tests' mounts.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Cause (2026-11-20, ubu1): a desktop session's trash monitor, gvfsd-trash (user ramon's GNOME session on ubu1), looks into every new mount: access(2) of its root, an inotify watch on it, .Trash and .Trash-1000 looked up; strace of the gvfs processes during the mount tests showed 76 such calls on the tests' mounts. With the diagnostic: 'shown before and after (old, old), asked of the filesystem Ok(new)': the server had the new times; the kernel's cache showed old ones, or (before) the new mtime with the old ctime. The server alone changes both times on every entry change (filesystem.rs every_change_of_entries_changes_both_times, 200 rounds). Cause: the same FUSE-client race as TASK-306: another process's GETATTR reply being applied (fuse_change_attributes_common sets mtime, then ctime, after marking the cache valid) while the test's stat reads the cache unlocked; the other process gvfsd-trash, whose inotify watch on the mount's root has it stat the root as each change lands there, hence the root alone. Fix: tests mount at .gvfs, which gvfs ignores (TASK-306's notes). Verified: fuse_differential 30/30 stress rounds on ubu1 (3/30 in each run before); the reproduction test changes_show_their_times_though_others_stat_the_nodes_meanwhile gained root create-then-mkdir checks. The forced-statx diagnostic stays in the differential's time failures.
<!-- SECTION:NOTES:END -->
