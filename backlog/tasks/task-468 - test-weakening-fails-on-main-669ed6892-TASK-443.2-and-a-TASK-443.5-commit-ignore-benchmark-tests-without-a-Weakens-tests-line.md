---
id: TASK-468
title: >-
  test-weakening fails on main: 669ed6892 (TASK-443.2) and a TASK-443.5 commit
  #[ignore] benchmark tests without a Weakens-tests: line
status: To Do
assignee: []
created_date: '2026-11-15 20:58'
labels:
  - bug
  - ci
dependencies: []
priority: high
ordinal: 875000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Main CI run 43758893191 (a252cb53f): job 'Commits that weaken checks say why' fails: 669ed6892 adds #[ignore = "a benchmark (TASK-443.2): --run-ignored only, in the sim profile"] at crates/cladefs/src/remote.rs:1083, and another commit #[ignore = "a benchmark (TASK-443.5): --run-ignored only, alone"] at crates/cladefs/src/remote/lifecycle.rs:540, neither message saying why in a 'Weakens-tests:' line (AGENTS.md). History on main can't be rewritten: find how the check accepts a reason after the fact (an allowlist or a later commit's line) and record that these are benchmarks, not tests around a bug; or move the benchmarks where the check doesn't count them.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
