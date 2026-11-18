---
id: TASK-599
title: fxfs-automount stops expiring mounts for good after one refused unmount
status: To Do
assignee: []
created_date: '2026-11-18 15:31'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 990000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), guest-kernel-runtime review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/fxfs-automount/src/linux.rs`: `rg -n -F 'eprintln!("fxfs-automount: expiring stopped: {error}");' crates/fxfs-automount/src/linux.rs`

Expirer::expire_one (autofs.rs) maps only EAGAIN, meaning nothing to expire, to Ok(false). Every other error is returned, and the expiry thread in run() logs it and returns, ending expiry for the rest of the process's life. The kernel's autofs_do_expire_multi returns autofs_wait's status for the expire it sent. When the daemon answers that Expire with AUTOFS_IOC_FAIL, which the main loop does whenever Automounter::expire returns false (unmount failed), autofs_wait_release sets the status to -ENOENT. AUTOFS_IOC_EXPIRE_MULTI then fails with ENOENT and the expirer thread exits. A single refused unmount, which autofs expects and handles by bumping last_used, permanently disables expiry. Idle mounts stay mounted and their volumes stay open with their memory held, contrary to the crate's contract ('A mount idle for the timeout expires').

**Trigger.** autofs picks an expiry candidate with may_umount_tree, which counts only that mount's own references, not copies propagated into other mount namespaces. The daemon's umount2(target, 0) goes through do_umount -> propagate_mount_busy, which also checks slave and peer copies. So: /repos/NAME is mounted. A process in a slave mount namespace, such as a systemd service with PrivateTmp/ProtectSystem or a container bind with rslave propagation, has its cwd or an open file inside its copy, while the original mount is idle for the timeout. The expirer's EXPIRE_MULTI selects the mount and sends Request::Expire. Volumes::unmount fails with EBUSY, so expire() returns false and map.answer(token, false) sends AUTOFS_IOC_FAIL. The ioctl returns ENOENT, expire_one returns Err, the thread logs 'expiring stopped: No such file or directory' and returns. From then on no mount under the root expires, even after the busy copy is released.

**Why the tests stay green.** daemon.rs's busy_mounts_stay test calls Automounter::expire directly on a Fake and never goes through Expirer::expire_one or the thread loop. The guest case test_fxfs_guest_automount only waits for idle mounts to expire once and never makes an expiry fail. autofs.rs has no test of the error mapping, which needs a real autofs mount.

**Backlog.** New. TASK-136.6 (Done) built expiry and tested only expiries that succeed; TASK-281 (Done) was a test race between umount2 and the volume closing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 After one expiry fails (the unmount is refused with EBUSY and EXPIRE_MULTI returns ENOENT), the expirer keeps running: once the busy copy is released, the mount expires within two timeouts and volumes_open drops.
- [ ] #2 A refused unmount never logs 'expiring stopped'.
- [ ] #3 The guest automount case, with /repos in a shared peer group, holds a repo busy from a slave mount namespace past the timeout, releases it and waits for it to expire; it goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
