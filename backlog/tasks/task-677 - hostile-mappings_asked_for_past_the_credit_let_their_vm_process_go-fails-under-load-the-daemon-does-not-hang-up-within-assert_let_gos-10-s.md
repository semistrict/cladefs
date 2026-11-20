---
id: TASK-677
title: >-
  hostile::mappings_asked_for_past_the_credit_let_their_vm_process_go fails
  under load: the daemon does not hang up within assert_let_go's 10 s
status: Done
assignee: []
created_date: '2026-11-20 00:27'
updated_date: '2026-11-20 06:21'
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
- [x] #1 The test passes in full-suite runs under load, or its bound is derived from what it waits for rather than a fixed 10 s, with why written in the test
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Seen again 2026-11-20 in two full ubu1 workspace runs under fxjob (--test-threads 4, scripts/test.sh clippy+workspace): on t3/replication-protocol-prior-art at 7153707a and on t3/sim-spike at 0ee8c498, both tests.rs:4119 'the daemon holds what the VM process leaves unread' (left 0, right 1), 17.7-17.9 s; passed alone right after (16.0 s). Both branches leave the hostile tests' code unchanged.

Cause measured: the let-go comes 8.7-8.8 s after the first MAPPINGS asked, alone, in a debug build on ubu1 (the daemon makes two 32 MiB replies of a million mappings before the third passes the credit), against assert_let_go's fixed 10 s. Fix (641bc6cde): assert_let_go takes its bound from its caller, tests.rs's WAIT (10 s) for its own cases, hostile.rs's WAIT (60 s, what would otherwise hang for good) for the hostile ones, with why in the test. Verified on ubu1 under fxjob: full workspace (scripts/test.sh, --test-threads 4) 3126/3126; cladefsd::tests at --test-threads 32 four times, the test passing each time it ran (other load flakes there noted on TASK-443.6, TASK-644, filed TASK-687); Mac clippy clean (the test is Linux-only); upstream-changes.py --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
assert_let_go's bound follows its case (hostile cases wait as their other steps do), the measured let-go being 8.8 s of the old fixed 10 s; ubu1 workspace 3126/3126.
<!-- SECTION:FINAL_SUMMARY:END -->
