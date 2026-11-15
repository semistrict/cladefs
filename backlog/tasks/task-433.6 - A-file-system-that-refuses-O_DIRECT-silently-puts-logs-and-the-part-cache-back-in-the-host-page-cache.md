---
id: TASK-433.6
title: >-
  A file system that refuses O_DIRECT silently puts logs and the part cache back
  in the host page cache
status: To Do
assignee: []
created_date: '2026-11-15 13:55'
labels: []
dependencies: []
references:
  - crates/extent_remote/src/direct.rs
  - crates/extent_store/src/dir.rs
  - crates/extent_remote/src/cache.rs
parent_task_id: TASK-433
priority: medium
type: bug
ordinal: 861000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the second-vendor review (Codex via scripts/review-change.sh, TASK-444.9) of 0ef6fa1b4. extent_remote::direct::bypass turns a failed fcntl(F_SETFL, O_DIRECT) into false; extent_store dir.rs HostDir::file (data = false) then returns a HostFile with small: None, and extent_remote cache.rs opened() returns (file, false): both carry on with buffered I/O, unbudgeted host page cache, with no error and no log line. direct.rs says so on purpose ("a file that cant bypass the cache is read and written as it is"), but AGENTS.md (no silent fallbacks to a costlier path, 2026-11-11) makes such a path a bug: refuse to open, so the setup gets fixed. Reachable on a writable file system without direct_IO (eCryptfs; F_SETFL returns EINVAL without FMODE_CAN_ODIRECT). The unaligned-I/O test accepts direct == false, so no test can see it. Decide per caller whether a store opened with direct I/O asked for should fail at open (likely), checked up front once per directory rather than per file.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A store or part cache opened with direct I/O on a file system that refuses it fails to open with an error naming the file system, rather than running buffered
- [ ] #2 A test injecting a failed bypass sees the open fail (it fails today)
- [ ] #3 direct.rs and the README say what happens where bypass is refused
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
