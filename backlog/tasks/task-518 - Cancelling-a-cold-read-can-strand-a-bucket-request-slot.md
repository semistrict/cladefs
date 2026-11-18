---
id: TASK-518
title: Cancelling a cold read can strand a bucket request slot
status: To Do
assignee: []
created_date: '2026-11-18 12:24'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 908000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of revision 845a6c123, finding 3.

In `crates/bucket_cell/src/fetch.rs:1034`, the tail cache owns a shared future even when its last reader goes away. If cancellation happens during the GET at line 1046, the slot stays in that suspended future. At a request limit of one, a different cold object can wait indefinitely. The daemon façade at `crates/cladefs/src/bucket.rs:481` spawns its inner read task, so dropping that outer handle alone leaves the GET running. The demonstrated cancellation applies to a directly polled fetch. Tie pending tail lifetime to its readers as part fetches already do at line 899. Verify both last-reader cancellation and one reader cancelling while another still awaits the same tail.

Isolated arm64 macOS validation. Use the tail and request-budget implementation with a manually completed bucket read and one slot. Cancel A's only reader during the GET, complete the underlying I/O and poll B. A still occupied the slot. B issued no GET until A was polled again. The probe exercised directly cancellable fetches.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 With a single GET slot, cancelling the last reader permits another cold key to proceed. Cancelling one of two readers preserves the remaining reader’s result.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
