---
id: TASK-500
title: >-
  test_nightly: a night measured again collides with the night before's results
  directory on a fast host (named to the second)
status: To Do
assignee: []
created_date: '2026-11-16 18:06'
updated_date: '2026-11-18 04:41'
labels:
  - scripts
  - nightly
  - bug
dependencies: []
priority: medium
ordinal: 889000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
scripts/tests/test_nightly.py's Nights.test_a_night_that_measured_nothing_is_measured_again fails every time on ubu1, on main 3934f820b as on the encryption branch: 'mkdir: .../docs/measurements/ubu1-nightly-2026-11-16T180549Z-03d770d9: File exists'. Both nights of the test run within one second there (2.5 s for the test), and TASK-489 names a night's results to the second with the head's commit, the same for both; on the Mac (10 s) they fall in different seconds and it passes.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A night's results directory names it apart from any other night of the same head, whatever the clock says
- [ ] #2 test_nightly passes on ubu1 and the Mac
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Seen again 2026-11-17 on ubu1 under fxjob (TASK-433.9's suite): mkdir ... ubu1-nightly-2026-11-18T044011Z-584de327: File exists.
<!-- SECTION:NOTES:END -->
