---
id: TASK-431
title: >-
  extent_store crash test recovers_from_its_bucket_with_and_without_its_disk:
  commits spanning segments at its bound (20 of 100 rounds, needs > 20)
status: To Do
assignee: []
created_date: '2026-11-15 09:26'
labels:
  - testing
  - flaky
dependencies: []
priority: high
ordinal: 822000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Main's CI test (workspace) on 7edc54ef3 (run 43682804204) failed crates/extent_store/tests/crash/main.rs: '20 commits spanned segments in 100 rounds' (assert spanning > KEPT_ROUNDS / 5); passed on 1fd0855fb. A coverage assertion sitting at its bound: find whether TASK-400 (parts packed ahead), TASK-407 or TASK-411 changed how often commits span segments, and make the test produce spanning commits deterministically enough (seeded workload sized for it) rather than lowering the bound.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
