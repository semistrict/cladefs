---
id: TASK-461
title: >-
  Mutant patches walks-forget-writes-in-flight and written-back-twice no longer
  apply on main: CI test (clippy scripts) fails
status: To Do
assignee: []
created_date: '2026-11-15 14:26'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 869000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Main CI run 43722206629 (9d814644b) failed test (clippy scripts) on its mutant checks: crates/cladefs/mutants/walks-forget-writes-in-flight.patch (crates/cladefs/src/extentd/disk.rs:202) and written-back-twice.patch (crates/extent_store/src/dir.rs:460) do not apply. Both applied at ab1adf06b (the rename); the files changed since in d2cfd3b4c (t3/tombstones merged into main) and 3139c4bdd (TASK-433.1/433.3, extent_store::pages), which did not update the patches.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Both patches apply to main and still produce the mutant they name, each killed by its test
- [ ] #2 The scripts part of scripts/test.sh passes on main
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
