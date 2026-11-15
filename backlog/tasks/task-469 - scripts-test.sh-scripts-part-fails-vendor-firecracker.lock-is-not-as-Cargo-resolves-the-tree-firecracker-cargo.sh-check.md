---
id: TASK-469
title: >-
  scripts/test.sh scripts part fails: vendor/firecracker.lock is not as Cargo
  resolves the tree (firecracker-cargo.sh --check)
status: To Do
assignee: []
created_date: '2026-11-15 23:37'
labels:
  - testing
  - bug
dependencies: []
priority: high
ordinal: 876000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-441's run of CLADEFS_TEST_PARTS=scripts scripts/test.sh on macOS (2026-11-15, branch t3/nightly-performance-reporting at 618403778, which touches no Cargo file): every unit test and mutant check passed, then scripts/firecracker-cargo.sh --check failed: "error: cannot update the lock file .../target/firecracker/Cargo.lock because --locked was passed" and "vendor/firecracker.lock is not as Cargo resolves the tree: scripts/firecracker-cargo.sh --lock updates it". vendor/firecracker.lock last changed in 815409fb8 (the rename, TASK-437); Cargo.lock changed since in 3b64d68ec (loom, TASK-444.2), 4d7e3190a (extent_files, TASK-456.1) and 5448054e3 (TASK-456.2), likely one of them changing what the Firecracker fork resolves of cladefs's crates. CI's scripts part fails the same way until the lock is regenerated (TASK-395 is why the check exists).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 scripts/firecracker-cargo.sh --check passes on main
- [ ] #2 The commit that left the lock stale is named in the notes, and why its own test run did not catch it
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
