---
id: TASK-677
title: >-
  hostile::mappings_asked_for_past_the_credit_let_their_vm_process_go fails
  under load: the daemon does not hang up within assert_let_go's 10 s
status: To Do
assignee: []
created_date: '2026-11-20 00:27'
updated_date: '2026-11-20 04:41'
labels:
  - bug
dependencies: []
ordinal: 1059000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen 2026-11-19 in a full `cargo nextest run -p cladefs -p bucket_cell --test-threads 4` on ubu1 (with another job running), at 86dede16a: the test took 17.9 s and failed at tests.rs:4117, poll returned 0 ("the daemon holds what the VM process leaves unread"). Passed 5 of 5 run alone right after (15.7 to 16.2 s each, so it already spends most of its time near the bound). Possibly related to TASK-546 (uncounted MAPPINGS replies). Not tracing: the hostile tests record no spans.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The test passes in full-suite runs under load, or its bound is derived from what it waits for rather than a fixed 10 s, with why written in the test
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Seen again 2026-11-20 in two full ubu1 workspace runs under fxjob (--test-threads 4, scripts/test.sh clippy+workspace): on t3/replication-protocol-prior-art at 7153707a and on t3/sim-spike at 0ee8c498, both tests.rs:4119 'the daemon holds what the VM process leaves unread' (left 0, right 1), 17.7-17.9 s; passed alone right after (16.0 s). Both branches leave the hostile tests' code unchanged.
<!-- SECTION:NOTES:END -->
