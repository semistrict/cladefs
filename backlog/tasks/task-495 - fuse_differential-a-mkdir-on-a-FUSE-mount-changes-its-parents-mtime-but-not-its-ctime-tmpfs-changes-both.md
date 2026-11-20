---
id: TASK-495
title: >-
  fuse_differential: a mkdir on a FUSE mount changes its parent's mtime but not
  its ctime (tmpfs changes both)
status: To Do
assignee: []
created_date: '2026-11-16 11:47'
updated_date: '2026-11-20 17:50'
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
- [ ] #1 The failure is reproduced from a fixed case (a regression entry) and its cause named: the mount or the comparison
- [ ] #2 The cause is fixed: a mkdir updates its parent's ctime as Linux filesystems do, or the comparison allows exactly the granularity the mount documents
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-18 (TASK-539's pre-push runs on ubu1): seen again as a symlink: 'call 6: Symlink { target: x, path: c }: of ., on the reference it mtime changed, ctime changed; on the mount, mtime changed, ctime stayed', once in 6 runs of the branch; and once as fusermount3 -u 'Device or resource busy' under the full suite's load. origin/main passed 3 of 3 and the branch 4 of 4 after, alone.

2026-11-20 (TASK-691 Linux run, ubu1, f4f4f4c78, fxjob 4 threads): seen again — random_calls_on_a_mount_come_to_what_they_do_on_tmpfs call 43 Mkdir { path: c, mode: 1032 }: of ., reference mtime and ctime changed, mount both stayed.
<!-- SECTION:NOTES:END -->
