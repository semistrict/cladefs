---
id: TASK-494
title: >-
  scripts/tests/test_rename_to_cladefsd.py fails: f531b4435's CLADEFSD_BUCKET
  rename reached the test's expected output
status: Done
assignee:
  - '@claude'
created_date: '2026-11-16 11:30'
updated_date: '2026-11-16 11:49'
labels:
  - bug
  - scripts
dependencies: []
priority: medium
ordinal: 884000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found running CLADEFS_TEST_PARTS=scripts scripts/test.sh for TASK-482: test_the_binary_variables_and_metrics_are_renamed fails (1 of 516). f531b4435 renamed CLADEFSD_REMOTE to CLADEFSD_BUCKET across the tree, including the expected output of scripts/tests/test_rename_to_cladefsd.py, but scripts/rename-to-cladefsd.sh renames CLADEFS_EXTENTD_REMOTE to CLADEFSD_REMOTE, as it must (its header: this script and its test keep their words). The expected string goes back to CLADEFSD_REMOTE.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 test_rename_to_cladefsd.py expects CLADEFSD_REMOTE, what rename-to-cladefsd.sh makes
- [x] #2 CLADEFS_TEST_PARTS=scripts scripts/test.sh passes
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Put the test's expected output back to CLADEFSD_REMOTE, what rename-to-cladefsd.sh makes.
2. Run CLADEFS_TEST_PARTS=scripts scripts/test.sh.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
scripts part: Ran 516 tests, OK (was 1 failure).
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Restored the expected CLADEFSD_REMOTE in test_rename_to_cladefsd.py, which f531b4435's bulk rename had changed; the script and its test keep their words. Verified: CLADEFS_TEST_PARTS=scripts scripts/test.sh, 516 tests OK.
<!-- SECTION:FINAL_SUMMARY:END -->
