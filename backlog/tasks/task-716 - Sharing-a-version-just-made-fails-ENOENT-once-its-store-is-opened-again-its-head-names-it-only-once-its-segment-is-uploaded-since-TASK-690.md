---
id: TASK-716
title: >-
  Sharing a version just made fails ENOENT once its store is opened again: its
  head names it only once its segment is uploaded (since TASK-690)
status: Done
assignee:
  - '@claude'
created_date: '2026-11-21 00:29'
updated_date: '2026-11-21 00:29'
labels:
  - bug
  - cladefsd
dependencies: []
priority: high
ordinal: 1111000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
cladefsd::tests::hostile::a_compromised_vm_process_harms_no_tenant_beside_it failed on ubu1 2026-11-20 (10 of 40 runs alone): make_version, drop the store, share_version -> Errno(ENOENT) (hostile.rs, control share_version). make_version returns before the cell's head names the version ('named once uploaded, behind'); the reopening wants the store and cuts its uploader short (TASK-216), whose last acknowledgement, paced to one head swap a second since TASK-690, is often still waiting; the reopened writer takes a head without the version, and Writer::share_version refuses it (Error::NoCell -> ENOENT). Bisected: b1407bac5 (before TASK-690) 0 of 40 ENOENT; 6cb6b16ee (after TASK-690, before TASK-691) 12 of 40; main 10 of 40.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A version just made, its store let go of and opened again, is shared, not refused
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Have OpenCell::share_version wait until every commit ordered so far is uploaded (make_uploaded) before it shares: the head then names the version.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Fix: OpenCell::share_version awaits make_uploaded first, as wait_uploaded does: a shared version is forked into other tenants' cells, so it must be in the bucket anyway. Verified on ubu1: the hostile test 40/40 stressed alone (10/40 failed before); macOS cladefs 393/393, clippy gate clean.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
share_version waits for the store's commits to be uploaded, so a version just made, its store opened again, is named by the head the reopened writer took. Verified: hostile test 40/40 on ubu1 (10/40 before).
<!-- SECTION:FINAL_SUMMARY:END -->
