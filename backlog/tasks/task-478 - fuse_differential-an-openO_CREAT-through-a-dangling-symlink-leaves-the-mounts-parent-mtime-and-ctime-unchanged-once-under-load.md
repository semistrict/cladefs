---
id: TASK-478
title: >-
  fuse_differential: an open(O_CREAT) through a dangling symlink leaves the
  mount's parent mtime and ctime unchanged, once, under load
status: To Do
assignee: []
created_date: '2026-11-16 04:02'
labels:
  - fuse
  - flake
dependencies: []
priority: medium
ordinal: 873000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
ubu1, branch t3/explore-tenant-encryption-keys at 96c1b85cd, workspace part (2886/2887 passed), random_calls_on_a_mount_come_to_what_they_do_on_tmpfs failed at fuse_differential.rs:1854: call 39 Symlink { target: a, path: c } then call 41 Open { path: c, flags: RDWR|CREAT, mode: 493 } -> Ok: of ., on the reference (tmpfs) mtime and ctime changed; on the mount both stayed. Proptest's minimal failing input was the whole 42-call case unshrunk, so no shorter case failed again: the outcome is not a function of the calls (timing: the kernel's attribute cache of the parent, or a time read before the create). The branch does not touch fuse/ or fxfs's times. Seen once; 9fc2eaa46's workspace run passed it.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A test makes the creation through a dangling symlink and checks the parent's times, deterministically
- [ ] #2 The cause is found and fixed, or the differential allows it with the reason
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
