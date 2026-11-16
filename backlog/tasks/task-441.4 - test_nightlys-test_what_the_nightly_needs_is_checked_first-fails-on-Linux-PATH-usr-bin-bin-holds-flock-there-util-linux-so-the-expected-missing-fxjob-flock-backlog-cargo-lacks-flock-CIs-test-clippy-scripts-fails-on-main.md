---
id: TASK-441.4
title: >-
  test_nightly's test_what_the_nightly_needs_is_checked_first fails on Linux:
  PATH=/usr/bin:/bin holds flock there (util-linux), so the expected 'missing
  fxjob flock backlog cargo' lacks flock; CI's test (clippy scripts) fails on
  main
status: Done
assignee:
  - '@claude'
created_date: '2026-11-16 01:58'
updated_date: '2026-11-16 02:39'
labels:
  - testing
  - bug
dependencies: []
parent_task_id: TASK-441
priority: high
ordinal: 877000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
main's CI runs 43776417313 and 43776797947 (66655cd02): test (clippy scripts) fails 'Tuples differ: (1, "... missing fxjob backlog cargo") != (1, "... missing fxjob flock backlog cargo")'. The test (682b719d7, TASK-441.2) narrows PATH to /usr/bin:/bin, which on macOS has no flock and on Linux does. The test's PATH should hold only what it means the script to find, on any host.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The test gives nightly-install.sh a PATH of its own holding only git and hostname, and expects the same message on macOS and Linux
- [x] #2 scripts/tests/test_nightly.py passes on macOS and on ubu1
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
6710dd2d2: the test links git and hostname into a directory of its own and runs nightly-install.sh with that alone as PATH. Passes on macOS (2.3 s); Linux in main's CI.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
test_what_the_nightly_needs_is_checked_first runs nightly-install.sh with a PATH of only git and hostname, so the same tools are missing on any host (6710dd2d2). Passes on macOS; main's CI run 43790612196 (Linux, scripts part) green.
<!-- SECTION:FINAL_SUMMARY:END -->
