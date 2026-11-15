---
id: TASK-466
title: >-
  TASK-433.5's CachedDir changed extent_store::dir's constructor; guest/tests
  test_cases no longer compiles (E0061): main's test (guest) and sandboxes jobs
  fail
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 20:58'
updated_date: '2026-11-15 21:15'
labels:
  - bug
  - ci
dependencies: []
priority: high
ordinal: 873000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Main CI run 43758893191 (a252cb53f): test (guest) and sandboxes (short) fail to build guest/tests' test_cases: error[E0061] 'this function takes 1 argument but 2 arguments were supplied' at test_cases/src/common.rs:447:15, calling the new() at crates/extent_store/src/dir.rs:213, changed by 38c8470dd (TASK-433.5: every store's directory holds its data files in the page cache, CachedDir). The guest tests' crate is outside the workspace, so the workspace build didn't catch it.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. guest/tests/test_cases/src/common.rs copy_store: HostDir::new(from) (the direct flag is gone since 38c8470dd; copy_store opens no file, only takes the directory).
2. Build guest/tests and run its harness tests (test_cases host, guest; runner) on ubu1: it builds on Linux alone (macOS: cladefs::extentd is Linux-only; a cross check needs a Linux C toolchain for zstd-sys).
3. scripts/test.sh: its guest part, on by default on Linux and CI's test (guest) job, already builds guest/tests; macOS can't, so no cheap addition there.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Cause: 38c8470dd (TASK-433.5) dropped HostDir::new's direct flag; guest/tests (outside the workspace, Linux-only: cladefs::extentd is cfg(linux)) still passed false, and TASK-433.5 was verified on macOS alone. Fix: HostDir::new(from) in copy_store (50a3c9d62), which opens no file, only takes the directory, so a DiskDir refusing a non-O_DIRECT file system changes nothing for it. guest/tests/Cargo.lock: cladefs's entry re-sorted as cargo writes it since the rename, no version changed.
Evidence: ubu1 (fxjob t466-guest, own worktree and target): guest/tests builds; nextest test_cases --features host 127/127, --features guest 99/99 (19 skipped: loop mounts need root), runner 9/9.
test.sh: not changed. Its guest part, default on Linux and CI's test (guest) job, already builds guest/tests, and that job caught this. On macOS it can't build (cladefs::extentd Linux-only); cargo check --target x86_64-unknown-linux-gnu fails at zstd-sys's build script without a Linux C cross compiler, so no cheap macOS check exists. The gap was a signature change verified on macOS only.
scripts/upstream-changes.py --check passes; nothing upstream touched (DoD #3: no deviation).
<!-- SECTION:NOTES:END -->
