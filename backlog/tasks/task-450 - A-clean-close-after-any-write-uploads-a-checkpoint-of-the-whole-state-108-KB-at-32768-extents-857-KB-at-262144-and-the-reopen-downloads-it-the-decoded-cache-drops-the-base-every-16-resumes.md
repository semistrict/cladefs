---
id: TASK-450
title: >-
  A clean close after any write uploads a checkpoint of the whole state (108 KB
  at 32,768 extents, 857 KB at 262,144), and the reopen downloads it; the
  decoded cache drops the base every 16 resumes
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
ordinal: 850000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Measured by TASK-443.14 (bucket_lifecycle_by_phase; docs/measurements/macos-bucket-lifecycle-2026-11-15). A fork that wrote one 4 KiB block (a 584-byte segment) and closed: 4 requests in series (a LIST of moves for each key prefix its extents are under, here the base's and its own; the checkpoint's create; the head's update) and a checkpoint of every extent it holds, inherited ones included: 4.0 KB at 1,024 extents, 107.6 KB at 32,768, 856.7 KB at 262,144 (~3.3 bytes an extent). Its reopen: 7 requests in 6 round trips and the same bytes down again, decoded (debug build: ~0.5 s of CPU at 262,144). A fork that wrote nothing closes for 0 requests. So a sandbox restored, written once, closed and deleted moves ~1 MB up and ~1 MB down for a base of 262,144 extents: at 10^5 such sandboxes a day, ~86 GB up and as much down a day, in checkpoints of state the base already has. The cache of decoded states (recovery::Decoded, KEEP 16) drops the oldest decoded, not the least used: with one popular base on a host and other stores resumed, the restore after every 16th resume reads the base's checkpoint again (15 requests instead of 14, 856 KB at 262,144), though every restore in between used it. TASK-242 (delta checkpoints) is the format change that would make the bytes proportional to what was written.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Closing a fork that wrote little uploads bytes in proportion to what it wrote, not to its base, and its reopen downloads no more, shown by bucket_lifecycle_by_phase at 1,024 and 32,768 extents
- [ ] #2 A base's decoded state that restores keep using stays decoded however many other stores are resumed between them, shown by the DECODED lines of the measurement
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
