---
id: TASK-522
title: A failed worker spawn can leave its caller waiting forever
status: To Do
assignee: []
created_date: '2026-11-18 12:24'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 912000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of revision 845a6c123, finding 7.

In `crates/cladefs/src/cladefsd/pool.rs:60`, the job enters the queue before the worker starts. If there are zero workers and spawn fails, the queue retains the reply sender and the caller at line 87 waits until another submission starts a worker. Exercise a single submission with a failed spawn and require an observable failure.

Evidence is from source inspection. Runtime reproduction is outstanding.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Failure to create the only worker resolves the pending request with an error without requiring a later submission.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
