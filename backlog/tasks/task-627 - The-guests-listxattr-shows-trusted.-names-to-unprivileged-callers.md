---
id: TASK-627
title: The guest's listxattr shows trusted.* names to unprivileged callers
status: To Do
assignee: []
created_date: '2026-11-18 15:35'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 1018000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), guest-vfs review; verified against the code by independent reviewers. Severity: low.

**Where:** `guest/linux/fs/cladefs/super.c`: `rg -n -F 'ret = cladefs_nofs(cladefs_listxattr(cladefs_vfs_fs(inode->i_sb), inode->i_ino, buf, size));' guest/linux/fs/cladefs/super.c`

cladefs_vfs_listxattr returns Fxfs's whole list of names: cladefs_listxattr in guest/rust/cladefs-linux/src/lib.rs concatenates every name list_xattrs returns. vfs_listxattr filters nothing when i_op->listxattr is set. ext4, xfs, btrfs and tmpfs list trusted.* names only to CAP_SYS_ADMIN, because the trusted handler's .list calls capable(). Here unprivileged users in the guest learn trusted xattr names, such as overlayfs's trusted.overlay.* on an upperdir kept on cladefs, or anything root set. Tools run as non-root (cp -a, rsync -X, tar --xattrs) then call getxattr on each listed trusted name and print EPERM for every file; on ext4 they stay silent.

**Trigger.** As root: setfattr -n trusted.secret -v 1 /mnt/x. As uid 1000: listxattr("/mnt/x") returns "trusted.secret" (getfattr -d -m - shows the name, and reading the value fails with EPERM). On ext4 the name is not listed.

**Why the tests stay green.** The guest xattr checks and xfstests' xattr tests run as root, and every filesystem lists trusted names to root.

**Backlog.** No related task. The host FUSE server's listxattr also lists every namespace unchecked, a separate surface.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 In the guest, with trusted.t and user.u set on a file by root, listxattr as uid 1000 without CAP_SYS_ADMIN returns exactly "user.u\0"; as root it returns both.
- [ ] #2 A guest check in test_fxfs_guest.rs does this with setresuid(1000) in a child and goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
