---
id: TASK-495
title: >-
  fuse_differential: a mkdir on a FUSE mount changes its parent's mtime but not
  its ctime (tmpfs changes both)
status: Done
assignee:
  - '@claude'
created_date: '2026-11-16 11:47'
updated_date: '2026-11-20 21:08'
labels:
  - bug
  - fuse
dependencies: []
priority: medium
ordinal: 885000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found 2026-11-16 on ubu1 (Linux workspace run of t493-int = main 4a3283a59 + TASK-493, which touches only extent_store's page cache and the soak): cladefs::fuse_differential random_calls_on_a_mount_come_to_what_they_do_on_tmpfs failed once: "call 26: Mkdir { path: c, mode: 2468 }: of ., on the reference it mtime changed, ctime changed; on the mount, mtime changed, ctime stayed". The case before it includes Fchmod of the root directory's fd at call 24 (mode 2468), which itself changes ctime; a ctime set by the fchmod and an mkdir in the same clock tick would read unchanged on a coarse clock, so this may be the test comparing times at a granularity the mount does not keep, or a real case where the mount's mkdir leaves its parent's ctime alone. The proptest's minimal case is in the run log (removed with the run); rerun with its seed to reproduce. TASK-306 is another fuse_differential flake.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The failure is reproduced from a fixed case (a regression entry) and its cause named: the mount or the comparison
- [x] #2 The cause is fixed: a mkdir updates its parent's ctime as Linux filesystems do, or the comparison allows exactly the granularity the mount documents
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Reproduce through the kernel: a mount test whose threads list and stat nodes while the main thread changes them (fuse_mount.rs).
2. Name the cause in the kernel's FUSE client.
3. Have the server tell the kernel (FUSE_NOTIFY_INVAL_INODE, attributes only) which nodes a request changed, before its reply.
4. Check the notifications without a kernel (tests/fuse.rs) and on ubu1 through it.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-18 (TASK-539's pre-push runs on ubu1): seen again as a symlink: 'call 6: Symlink { target: x, path: c }: of ., on the reference it mtime changed, ctime changed; on the mount, mtime changed, ctime stayed', once in 6 runs of the branch; and once as fusermount3 -u 'Device or resource busy' under the full suite's load. origin/main passed 3 of 3 and the branch 4 of 4 after, alone.

2026-11-20 (TASK-691 Linux run, ubu1, f4f4f4c78, fxjob 4 threads): seen again — random_calls_on_a_mount_come_to_what_they_do_on_tmpfs call 43 Mkdir { path: c, mode: 1032 }: of ., reference mtime and ctime changed, mount both stayed.

Cause (2026-11-20): Linux's FUSE client marks a directory stale as its entries change (fuse_dir_changed), a renamed node's or an xattr-changed node's ctime (fuse_update_ctime), but leaves the inode's attr_version alone: a GETATTR reply the server sent before the change, arriving after, passes fuse_change_attributes_i's attr_version check, clears inval_mask and is cached for the mount's attr_valid (24 h). The mount; not the comparison. Server granularity is not it: Fxfs and the server stamp times with SystemTime::now(). Fix: Server::changed sends FUSE_NOTIFY_INVAL_INODE (off -1) for each node a request changed (setattr; symlink, mknod, mkdir, create, unlink, rmdir, link's directory; rename's two directories and the moved node; setxattr, removexattr), after it and before its reply; fuse_reverse_inval_inode moves attr_version on, so no reply asked before is taken as fresh. ENOENT (node not held) ignored, other errors logged. Both shapes now have a notifier (Server::new takes one; mount.rs's notifier()). Filesystem::rename returns the moved node's inode. Kernel locking checked (7.1): fc->killsb is held only briefly, so notifying from a handler cannot deadlock. Reproduced: fuse_mount changes_show_their_times_though_others_stat_the_nodes_meanwhile (8 listing/statting threads, CPUs oversubscribed by spinners), 4 stale renames in 350 rounds / 20 s on ubu1 before; after, passes with root chmod and symlink checks added. tests/fuse.rs changes_tell_the_kernel_the_nodes_they_changed checks which nodes each request reports. ubu1: fuse + fuse_mount 30/30; fuse_differential 10/10 stress rounds. Remaining, filed: TASK-706 (a root symlink's mixed mtime/ctime, seen once after the fix, not reproduced), TASK-705 (fusermount3 -u busy), TASK-306 (root mode 0, seen once).

Verified 2026-11-20 at 6cdd8528a: ubu1 workspace (scripts/test.sh workspace, fxjob, 4 threads) 3159/3159; VMM and guest builds clean; macOS workspace 2794/2794; upstream-changes --check passes; clippy gate clean.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Stale directory (and renamed or xattr-changed node) times on FUSE mounts came from Linux's FUSE client taking a GETATTR reply sent before a change as fresh, the change having left attr_version alone. The server now sends FUSE_NOTIFY_INVAL_INODE for each node a request changed, before its reply. Reproduced and verified through the kernel (fuse_mount.rs), checked without one (tests/fuse.rs); fuse_differential 10/10 stress rounds; workspaces pass on macOS and Linux. Follow-ups filed: TASK-705, TASK-706.
<!-- SECTION:FINAL_SUMMARY:END -->
