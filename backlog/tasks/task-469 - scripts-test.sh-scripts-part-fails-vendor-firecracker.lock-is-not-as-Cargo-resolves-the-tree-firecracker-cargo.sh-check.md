---
id: TASK-469
title: >-
  scripts/test.sh scripts part fails: vendor/firecracker.lock is not as Cargo
  resolves the tree (firecracker-cargo.sh --check)
status: Done
assignee:
  - '@claude'
created_date: '2026-11-15 23:37'
updated_date: '2026-11-16 02:39'
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
- [x] #1 scripts/firecracker-cargo.sh --check passes on main
- [x] #2 The commit that left the lock stale is named in the notes, and why its own test run did not catch it
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Stale since 5448054e3 (TASK-456.2): crates/cladefs/Cargo.toml took extent_files (4d7e3190a's crate), so the fork's tree resolves one package more. Its run was CLADEFS_TEST_PARTS='clippy workspace go' on macOS and clippy + workspace on ubu1; firecracker-cargo.sh --check runs in the scripts part, which neither ran. Fixed in 6710dd2d2: firecracker-cargo.sh --lock adds extent_files and lists cladefs's dependencies with it; --check passes on macOS after, failed before.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
vendor/firecracker.lock regenerated with extent_files (6710dd2d2), which cladefs took in 5448054e3 without the lock; firecracker-cargo.sh --check passes on macOS, and main's CI run 43790612196 on 6710dd2d2 is green in every job (firecracker, sandboxes short, scripts part, Linux and macOS workspace).
<!-- SECTION:FINAL_SUMMARY:END -->
