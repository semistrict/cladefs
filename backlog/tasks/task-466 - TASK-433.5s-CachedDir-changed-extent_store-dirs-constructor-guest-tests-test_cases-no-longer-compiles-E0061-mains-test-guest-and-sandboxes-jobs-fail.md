---
id: TASK-466
title: >-
  TASK-433.5's CachedDir changed extent_store::dir's constructor; guest/tests
  test_cases no longer compiles (E0061): main's test (guest) and sandboxes jobs
  fail
status: To Do
assignee: []
created_date: '2026-11-15 20:58'
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
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
