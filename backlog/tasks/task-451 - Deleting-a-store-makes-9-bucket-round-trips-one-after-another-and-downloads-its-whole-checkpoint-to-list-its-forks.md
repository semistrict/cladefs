---
id: TASK-451
title: >-
  Deleting a store makes 9 bucket round trips one after another and downloads
  its whole checkpoint to list its forks
status: To Do
assignee: []
created_date: '2026-11-15 12:58'
labels:
  - perf
  - review-2026-11-15
dependencies: []
references:
  - TASK-443.5
  - TASK-443.14
documentation:
  - docs/measurements/macos-bucket-lifecycle-2026-11-15/note.md
priority: medium
type: enhancement
ordinal: 851000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Measured by TASK-443.14 (bucket_lifecycle_by_phase; docs/measurements/macos-bucket-lifecycle-2026-11-15). Deleting a closed fork that wrote one block: 9 requests, each after the last, 286 ms at 30 ms a request, 917 ms at 100 ms: the head read, the cell read, the head ended, the record read, the checkpoint read (107.9 KB at 32,768 extents, 857 KB at 262,144), the record read again, its update, the cell read again, its update. The checkpoint is recovered (branch::delete_under -> recover_from) only for the state's forks; a store with a snapshot makes one request more (its child's record). A fork that wrote nothing makes the same 9 requests and reads its base's state instead, from the decoded cache if it is there, the base's checkpoint if not. With close, a sandbox that lived minutes pays 13 round trips and ~2x its checkpoint to go away. Not measured here: a killed VM's upload drain before the delete (TASK-443.14, Linux).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Deleting a store downloads nothing that grows with its state or its base, shown by bucket_lifecycle_by_phase at 1,024 and 32,768 extents
- [ ] #2 A deletion makes at most half the round trips in series it does today (9), pinned by a test counting its requests
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
