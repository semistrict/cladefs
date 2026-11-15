---
id: TASK-443
title: >-
  Performance risks for agent sandboxes found by the 2026-11-15 review of main:
  investigate and size each
status: To Do
assignee: []
created_date: '2026-11-15 12:24'
labels:
  - perf
  - review-2026-11-15
dependencies: []
documentation:
  - docs/reviews/2026-11-15-sandbox-performance.md
priority: high
ordinal: 834000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seven read-only reviews of main (80138a886) against LangSmith's agent-sandbox workload (hundreds of VMs a host, forks of a few bases, high churn, one shared SSD, a bucket tens of ms away) found where the code will run into performance problems; docs/reviews/2026-11-15-sandbox-performance.md ranks them. Nothing was built or run: every size is an estimate from the code. Each subtask is an investigation: measure the cost, say at what workload size it bites, and file the fix as its own task if it does. What these find decides what the nightly on ubu1 (TASK-441) measures.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each subtask says what it measured, or that the problem is not real, with the numbers
- [ ] #2 The review's ranking in docs/reviews/2026-11-15-sandbox-performance.md is corrected where a measurement contradicts it
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
