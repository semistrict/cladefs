---
id: TASK-443.14
title: >-
  Investigate: a short-lived sandbox uploads all it wrote and a whole-state
  checkpoint before it can be deleted; snapshot and delete pay for the
  checkpoint again
status: To Do
assignee: []
created_date: '2026-11-15 12:26'
labels:
  - perf
  - review-2026-11-15
dependencies: []
documentation:
  - docs/reviews/2026-11-15-sandbox-performance.md
parent_task_id: TASK-443
priority: medium
ordinal: 848000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/fxdax/src/store_table.rs cuts a closing store's upload drain short for a Claim::Writer alone, so a delete waits CLOSING_WAIT and returns EBUSY while the dead VM's store uploads up to its lag bound; crates/extent_store/src/upload.rs writes a checkpoint of the whole state, inherited extents included, on every clean stop, after one serial LIST per key prefix (MovesMaps::read). A resume then downloads and decodes that checkpoint (recovery.rs keeps 16 decoded, first in first out); branch::delete_under recovers the whole state to list forks; a snapshot waits for the backlog with the VM paused (take_snapshot -> wait_uploaded), then re-reads its own state and writes its checkpoint twice (recovery::checkpoint_at, writer.checkpoint). Found by reading the code, not measured (docs/reviews/2026-11-15-sandbox-performance.md).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 For a fork of a realistic base: write 500 MiB, kill the VM, delete (time to success, bytes uploaded after the kill); open, one write, close (bucket bytes and requests); snapshot after N GiB written (upload_wait); delete (downloaded bytes)
- [ ] #2 The notes give the measured cost and the workload size at which a customer would feel it; a fix, if needed, is filed as its own task
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
