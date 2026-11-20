---
id: TASK-705
title: >-
  fuse_differential: unmounting at a case's end fails, fusermount3 -u 'Device or
  resource busy', now and then under load
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-20 20:33'
updated_date: '2026-11-20 21:09'
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
