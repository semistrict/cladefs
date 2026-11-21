---
id: TASK-306
title: >-
  fuse_differential: a new store's mounted root reads mode 0 instead of 0755,
  now and then, under load
status: Done
assignee:
  - '@claude'
created_date: '2026-11-09 13:04'
updated_date: '2026-11-21 02:00'
labels:
  - fuse
  - flaky
  - fxfs
dependencies: []
priority: medium
ordinal: 717000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen 2026-11-09 on ubu1 (x86_64, non-root, fusermount3), TASK-302's branch d68181268, during a full `scripts/test.sh` workspace run: random_calls_on_a_mount_come_to_what_they_do_on_tmpfs failed at call 0 with "the roots differ": reference 0755 uid 1000 nlink 2, mount perm 0 uid 1000 nlink 2. The uid/nlink are the server's (the kernel's placeholder root, rootmode=40000, has uid 0 nlink 1), so the server answered mode 0 for a root just formatted and given the run's owner by Node::setattr(uid, gid) (Case::new): the root, which has no POSIX attributes of its own until then, came back without its 0755. Shrinking kept a 60-op input, so the failure did not recur on retries. Six runs of fuse_differential + fuse_mount alone passed (26-37 s each); the earlier run passed too.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The root's mode after a setattr of uid/gid alone is 0755 always: a test that formats and setattrs many times, concurrently with other filesystems' work, checks it
- [x] #2 The cause is found and fixed
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (GCE, scripts/test-gce.sh)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Test the root's mode after format + setattr, many at once, without a kernel and through it. 2. Name the cause. 3. Keep host processes from statting the tests' mounts as the tests read them.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Seen again 2026-11-17 on ubu1 (fxjob, test-threads 4), random_calls_on_a_mount_come_to_what_they_do_on_tmpfs at call 0: mount root perm 0 uid 0 against 0755 uid 1000; tree 073feb370 + TASK-512/433.9/433.10 (page cache frames).

2026-11-20 seen again on ubu1 (TASK-495's verification, fuse_differential --stress-count 10): 'call 0: the roots differ: . differs: reference perm 493, mount perm 0', 1 of 10 rounds.

Cause (2026-11-20, ubu1): a desktop session's trash monitor, gvfsd-trash (user ramon's GNOME session on ubu1), looks into every new mount: access(2) of its root, an inotify watch on it, .Trash and .Trash-1000 looked up; strace of the gvfs processes during the mount tests showed 76 such calls on the tests' mounts. Cause of the mode 0: Linux's FUSE client (7.1 fs/fuse/inode.c fuse_change_attributes_common) marks the cache valid (inval_mask cleared, i_time set) before it writes the fields, under fi->lock, while a stat answered from the cache (fuse_update_get_attr's generic_fillattr, stat->mode = fi->orig_i_mode) reads them without it: a stat racing another process's reply to a new root reads the server's uid and nlink with the placeholder's orig_i_mode (rootmode=40000, perm 0), exactly this task's 'perm 0, uid 1000, nlink 2'. The other process: gvfsd-trash's access(2) of each new mount's root. The server is right: new_roots_given_owners_keep_their_permissions (filesystem.rs, 8 threads formatting and setting owners) passes. Fix: the tests mount at a directory named .gvfs, which GLib takes for system-internal (g_unix_is_mount_path_system_internal: a path ending /.gvfs) and gvfs's trash watcher ignores; strace showed no gvfs call on them after (76 before). Test: fuse_mount new_roots_read_their_permissions_through_the_kernel (200 mounts by 8 threads, mode/uid/gid/nlink each). Verified on ubu1: fuse_mount 12/12, fuse_differential 30/30 stress rounds (3 of 30 failed in each of three runs before, across the three tasks).

Verified 2026-11-20 at b4b6dd253: ubu1 fuse_differential + fuse_mount 50/50 stress rounds (12 tests each); macOS workspace 2796/2796, clippy gate clean, upstream-changes --check passes; ubu1 workspace 3161/3162 at the same FUSE code, the one failure TASK-716's, fixed (hostile test 40/40 after).
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
A new root read mode 0 through the kernel when gvfsd-trash's access(2) of each new mount raced the test's stat: Linux's FUSE client marks its cache valid before writing the fields (orig_i_mode last), so a cached stat read the placeholder mode beside the server's owner. Tests mount at .gvfs, which gvfs ignores. Verified: 50/50 FUSE stress rounds on ubu1; new tests format+setattr concurrently (no kernel) and mount 200 roots.
<!-- SECTION:FINAL_SUMMARY:END -->
