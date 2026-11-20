---
id: TASK-698.7
title: >-
  Forgetting a shredded tenant or cell does not reach an open ephemeral cell:
  its ring is held nowhere among the host keys
status: Done
assignee: []
created_date: '2026-11-20 18:09'
updated_date: '2026-11-20 20:14'
labels: []
dependencies: []
parent_task_id: TASK-698
priority: high
ordinal: 1097000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
upload::open_ephemeral builds the ephemeral cell ring (keys::ring) and hands it to the store, but holds it in no RingHold of the host keys::Host, as an Uploader does for bucket cells (Shared.held). keys::Host::forget, which ForgetTenantKeys/ShredCell reach, therefore cannot drop the keys of an open ephemeral cell, and its store keeps reading and writing under a shredded tenant keys until closed (doc-16, TASK-637).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 An open ephemeral cell holds its ring among the host keys while open, so the host fetchers read what it inherits and forgetting its tenant or cell drops those keys
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-20: done for the ring: upload::open_ephemeral returns a RingHold the OpenCell keeps (OpenCell.keys). Verified by ephemeral_cells_migrate_live_between_hosts, whose target read nothing it inherits before the ring was held (EIO, the parent DEK not held). Halting an open ephemeral store on forget split to TASK-704.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Ephemeral stores hold their ring among the host keys while open, which the host fetchers read inherited extents with and forget drops. Halting the store itself on forget is TASK-704.
<!-- SECTION:FINAL_SUMMARY:END -->
