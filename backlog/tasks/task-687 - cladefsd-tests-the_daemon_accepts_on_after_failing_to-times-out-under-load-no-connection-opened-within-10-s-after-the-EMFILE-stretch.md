---
id: TASK-687
title: >-
  cladefsd::tests::the_daemon_accepts_on_after_failing_to times out under load:
  no connection opened within 10 s after the EMFILE stretch
status: To Do
assignee: []
created_date: '2026-11-20 06:03'
updated_date: '2026-11-20 07:09'
labels:
  - testing
  - flaky
dependencies: []
ordinal: 1077000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen 2026-11-20 on ubu1 (load ~15 on 16 cores) running cladefsd::tests at --test-threads 32 under fxjob, at main 05299a254 plus a test-only change (t3/task-677): the test failed once at tests.rs:3814, 'the daemon accepts no more: Err(Timeout)': after serving began with no file to open (RLIMIT_NOFILE 0 for 1 s) and the limit was set back, CellClient::connect of a new store did not finish within 10 s. Passed in the other loaded runs and alone. Unknown whether the daemon's accept loop backs off for long after EMFILE (TASK-79.35) or the store's making is slow under the load; the 10 s bound covers both.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Why the connection took past 10 s is known (the accept loop's retry after EMFILE, or the store's making), and the test's bound follows from it, or the daemon accepts promptly once files close
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-20 investigation: the accept loop retries every 50 ms after EMFILE (ACCEPT_AGAIN), so it is not the wait. The opening after the stretch takes 50-120 ms on ubu1 alone, 63 ms in a loaded run (cladefsd::tests at 32 threads): the failed run's 10 s is a hang, not slowness. The failing run logged only the one EMFILE line. Not reproduced: 300 runs (--stress-count 300 --test-threads 32, RUST_LOG=debug) all passed; two more loaded runs of cladefsd::tests passed. c85b229a1: the test now logs every thread's stack (cladefs::stacks::log_stacks, eu-stack) if the opening doesn't come, checked to print on ubu1 by forcing it once. Left open until it fails again with stacks.
<!-- SECTION:NOTES:END -->
