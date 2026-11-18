---
id: TASK-600
title: 'The guest never caches symlink targets, so every traversal leaves RCU walk'
status: To Do
assignee: []
created_date: '2026-11-18 15:31'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 991000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), guest-vfs review; verified against the code by independent reviewers. Severity: medium.

**Where:** `guest/linux/fs/cladefs/super.c`: `rg -n -F 'target = kmalloc(PATH_MAX, GFP_KERNEL);' guest/linux/fs/cladefs/super.c`

cladefs_vfs_get_link returns -ECHILD in RCU walk (dentry NULL). In ref-walk it allocates PATH_MAX bytes, takes the fill barrier on followers, and calls cladefs_readlink. That call does a block_on, which allocates a boxed future, an Arc'd parker and a C parker (cladefs_rt_parker_new), then a tree lookup. kfree_link frees the buffer when the path is put. All of this happens on every traversal. cladefs_vfs_iget_as never sets inode->i_link (inode_set_cached_link) for S_IFLNK, so pick_link never takes its READ_ONCE(inode->i_link) fast path. Caching is safe: a symlink's target cannot change for its inode, on a writer or a follower. On a merged-/usr root (/bin, /lib, /lib64 -> usr/...) each exec crosses several symlinks: the ELF interpreter /lib64/ld-linux..., libraries opened through /lib64, and /etc/alternatives chains. Each crossing forces try_to_unlazy, which takes refcounts and d_lockref on the shared prefix dentries (contended under parallel builds), plus the Rust call.

**Trigger.** The guest's root is a cladefs volume imported from a merged-usr image. Run make -j$(nproc) or any exec-heavy workload. Each execve resolves /lib64/ld-linux-x86-64.so.2: pick_link (RCU) calls cladefs_vfs_get_link(NULL), which returns -ECHILD; try_to_unlazy follows; then cladefs_vfs_get_link(dentry) does kmalloc(4096), block_on(cladefs_readlink) and kfree. This repeats on every exec and for every library resolved through /lib64.

**Why the tests stay green.** Symlink checks verify targets only. No benchmark measures path resolution through a symlink or the exec rate, and the cost has no functional symptom.

**Backlog.** No related task covers it: TASK-25.9 (Done) cached symlinks on the FUSE path only; TASK-230 (Done) closed the warm-walk gap to ext4 on virtio-pmem for directory reads and clocks, not symlink traversal.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A symlink inode's target is read from Fxfs once per inode, on writers and followers alike; later traversals resolve it in RCU walk with no cladefs_readlink call and no allocation.
- [ ] #2 A guest bench of 1M stat() calls through a symlink makes one cladefs_readlink per symlink inode and is measured against ext4 on virtio-pmem.
- [ ] #3 A regression guest case counts cladefs_readlink calls (a stats counter or the trace) over repeated traversals of one symlink, expects 1, and goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
