---
id: TASK-626
title: A follower batch detaches mounts on unchanged subdirectories
status: To Do
assignee: []
created_date: '2026-11-18 15:35'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 1017000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), guest-vfs review; verified against the code by independent reviewers. Severity: low.

**Where:** `guest/linux/fs/cladefs/super.c`: `rg -n -F 'return READ_ONCE(dentry->d_time) == (unsigned long)atomic64_read(&cladefs_vfs_inode(dir)->gen);' guest/linux/fs/cladefs/super.c`

cladefs_vfs_d_revalidate returns 0 for every child whose d_time is older than its parent's generation, without looking the name up again. A name that still links the same inode is therefore reported invalid. lookup_fast then calls d_invalidate, which __d_drop()s the dentry, prunes its cached subtree, and calls detach_mounts() on anything mounted on it or below. NFS and FUSE, which the comment cites as its model, look the name up again and return 1 when it still maps to the same inode, so their submounts survive unrelated changes. Also, until the next lookup re-splices the dropped directory dentry, d_unlinked() is true for it, so a concurrent getcwd() inside it fails with ENOENT.

**Trigger.** A follower is mounted at /f, with mount -t tmpfs none /f/sub (or a bind mount, or the /proc of a chroot rooted on the follower). The writer creates or removes any file in its root directory and commits. The follower's batch runs cladefs_vfs_drop_entries(root), which bumps the root's gen. The next ls /f/sub goes through lookup_fast: d_revalidate(sub) returns 0, d_invalidate(sub) calls detach_mounts, and the tmpfs is lazily unmounted. ls now shows the follower's own sub.

**Why the tests stay green.** The follower cases (follow torture, history checker) compare data, attributes and entries with the writer's. None mounts anything on a follower directory or checks that cached dentries survive. TASK-92.31 measured only the cost of the extra lookups.

**Backlog.** Related: TASK-92.31 (closed won't-fix on 2026-11-08, for performance alone) has the same root cause, d_revalidate failing a stale stamp without a lookup, but never weighed detached mounts or getcwd. TASK-42.6 (Done) specified the stamps.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 When the writer creates or removes a file in a follower directory and the batch settles, a tmpfs or bind mount on an unchanged subdirectory stays mounted and its files stay visible. A name that still links the same inode revalidates as valid.
- [ ] #2 A follower case mounts a tmpfs on a follower subdirectory and writes a marker file in it. The writer creates a file in the follower's root, and the case waits until CLADEFS_IOC_FOLLOW_COMMITS shows a settled commit past it. It asserts the marker is still visible through the subdirectory and /proc/self/mountinfo still lists the tmpfs, and it goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
