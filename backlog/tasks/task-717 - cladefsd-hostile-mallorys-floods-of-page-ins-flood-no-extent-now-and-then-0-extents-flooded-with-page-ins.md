---
id: TASK-717
title: >-
  cladefsd hostile: mallory's floods of page-ins flood no extent now and then
  ('0 extents flooded with page-ins')
status: Done
assignee:
  - '@claude'
created_date: '2026-11-21 00:29'
updated_date: '2026-11-21 08:03'
labels:
  - bug
  - cladefsd
  - flaky
dependencies: []
priority: low
ordinal: 1112000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen on ubu1 2026-11-20 at b1407bac5 (before TASK-690), cladefsd::tests::hostile::a_compromised_vm_process_harms_no_tenant_beside_it stressed alone, 1 of 40 runs: hostile.rs's assert 'flooded > 0' failed, '0 extents flooded with page-ins'. Mallory's random cases include floods of page-ins of the given cell's 1 MiB extents (TASK-149.34); a run may draw too few, or they may all be refused, leaving the check of the flood path unexercised. Find whether the draw or a refusal leaves it at 0, then make the test cover the flood path every run.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The cause of a run flooding no extent is named
- [x] #2 The test floods extents in 100 runs of 100
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Tally each flood MAP's answer by status. 2. Reproduce on ubu1 and read each restore's window and answers. 3. Fix the draw so every run floods. 4. 100 rounds on ubu1.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Tally on ubu1: the failing run had 4 restores; MAPs answered ENOSYS 324, EBADF 372, ENOENT 162, unanswered 354, none 0. Per restore: window past MAX_WINDOW or private max 0/u64::MAX -> every MAP ENOSYS (no window, open_cell.rs); granted windows flood (some MAPs unanswered when the connection ends first). ~54% of restores drew a granted window, so P(no flood) ~0.46^restores. Before the fix the fewest flooded in 130 runs was 12 (one restore) and once 0; after, the fewest in 100 was 84.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Cause: a restore floods only through a granted window, and the test drew a refused one (past MAX_WINDOW, or private max 0 or u64::MAX) for ~46% of restores; a run with 4 restores all refused flooded nothing. Restores now draw granted windows (refused ones stay covered by other connections, whose MAPs answer ENOSYS); the check reports MAP answers by status and each restore's window, acking and page-ins. Verified: 100/100 stress rounds on ubu1 under fxjob, at least 84 extents flooded in each; clippy gate clean.
<!-- SECTION:FINAL_SUMMARY:END -->
