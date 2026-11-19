---
id: TASK-659
title: >-
  fxfs-guest-pgbench and -dax fail doc-12's commit latency on ubu1: under half
  of commits durable within 50 ms
status: To Do
assignee: []
created_date: '2026-11-19 17:21'
labels:
  - test-host
  - guest
  - performance
dependencies: []
priority: medium
type: bug
ordinal: 1038000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
On ubu1 (2026-11-19) fxfs-guest-pgbench and fxfs-guest-pgbench-dax fail their host side's check of doc-12's median commit latency (50 ms): main at 258e6b9bf, 316239 of 685733 commits durable within 50 ms; branch t3/mirrors at 251bfc560, 275167 of 666966 and 265716 of 664345 (twice, -dax too). The cladefsd log shows every flush's time in its log files' fsync (logs 40-60 ms, raw files the same), segments framed durable in 50-90 ms. Earlier runs of the same branch (50933bf33, 439a7068c, the 18th's main) passed the case. Either the test host's scratch disk drifted (it has before: zvol scratch drifting 4x within a job), or main's commits since 348c59327 (TASK-550/563/569/575/582/609/611/612: one gate for changes to a store, lag bound for every level) slowed commits' durability. Replay: CLADEFS_WORKSPACE_TESTS=0 CLADEFS_E2E_TEST=fxfs-guest-pgbench CLADEFS_TEST_HOST_COMMIT=<commit> scripts/test-host.sh RESULTS; compare 348c59327 with main's head on the same host, back to back.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Main's head and 348c59327 measured back to back on ubu1: whether main's commits since slowed commits' durability
- [ ] #2 The case passes on ubu1 at main's head, or its bound is shown to be the host's
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
