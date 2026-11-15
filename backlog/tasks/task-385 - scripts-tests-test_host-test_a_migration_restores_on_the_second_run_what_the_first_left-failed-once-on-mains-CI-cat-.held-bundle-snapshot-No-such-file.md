---
id: TASK-385
title: >-
  scripts/tests test_host
  test_a_migration_restores_on_the_second_run_what_the_first_left failed once on
  main's CI: cat .held/bundle/snapshot: No such file
status: Done
assignee:
  - '@claude'
created_date: '2026-11-12 14:00'
updated_date: '2026-11-14 23:49'
labels:
  - bug
  - ci
  - scripts
dependencies: []
priority: medium
type: bug
ordinal: 764000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Main CI run 43231642989 (26241e965, TASK-380's fix, which touches no script), job 'test (clippy scripts)': scripts/tests/test_host.py:1005 Drivers.test_a_migration_restores_on_the_second_run_what_the_first_left: status 1, 'cat: /mnt/tmp/tmp.../scratch/run-20261112T135539Z-c241fd05.held/bundle/snapshot: No such file or directory'. Passed on the three runs before (a8ba9e944, 0c2d489a4, 3a9c5181e), so a race in the fake host's migrate driver (migrate-host.sh with FXDAX_FAKE_HOST=migrate) or its held bundle, not a change.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The race found and fixed, the test passing 200 times in a row
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Find the race: CI's 2026-11-14 failure was 'nice: sudo: Text file busy'; fake_ssh.py's tools() rewrote host-bin's tools in place on every ssh call, the wait call's while the launched job exec'd sudo.
2. Write each tool under a name of the call's, then rename it into place.
3. Prove: widened write window red 5/5 before, green after; 200+ loops under load before/after.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-14, coordinator: again on main 3a01ee0ab (run 43627290622, test (clippy scripts)): test_host.Drivers.test_a_migration_restores_on_the_second_run_what_the_first_left.

2026-11-14 (claude): cause in the test's fake host: fake_ssh.py's tools() rewrote host-bin's tools in place on every ssh call; host_wait's call ran while the job host_job launched exec'd sudo (nice -n 19 sudo ...): empty, sh ran it as nothing, exit 0, the source's host script never ran (the 2026-11-12 'No such file'), or open for writing on Linux ('nice: sudo: Text file busy', 2026-11-14). Fixed in 641d0d5ff: each tool written under a name of the call's, then renamed into place. Before: 1/200 failed (10 at once beside 15 busy loops, macOS); write window widened to 0.2 s: 5/5 failed (ran.txt missing: the host script never ran). After: 0/400, and 5/5 passed widened. test.sh scripts part 317 tests OK on macOS; Linux left to main's CI.

2026-11-14, coordinator: main's CI green on efa48fa88 (run 43653116878): test (workspace) on Linux and macos (workspace) both pass, with this fix in.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
fake_ssh.py rewrote the fake hosts' tools in place on every ssh call, racing the launched job exec'ing sudo (empty: the host script never ran; Linux: ETXTBSY). Tools are now written then renamed into place. 1/200 failing before, 0/400 after; widened window 5/5 failing before, 0/5 after.
<!-- SECTION:FINAL_SUMMARY:END -->
