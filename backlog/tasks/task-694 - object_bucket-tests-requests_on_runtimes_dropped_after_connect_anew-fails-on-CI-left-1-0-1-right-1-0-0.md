---
id: TASK-694
title: >-
  object_bucket::tests::requests_on_runtimes_dropped_after_connect_anew fails on
  CI: left [1, 0, 1], right [1, 0, 0]
status: To Do
assignee: []
created_date: '2026-11-20 13:58'
labels: []
dependencies: []
priority: medium
ordinal: 1084000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
main CI run on 305aa2033 (test (workspace)): panicked at crates/bucket_cell/src/object_bucket.rs:3617:9, assertion left == right failed, left [1, 0, 1], right [1, 0, 0].
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
