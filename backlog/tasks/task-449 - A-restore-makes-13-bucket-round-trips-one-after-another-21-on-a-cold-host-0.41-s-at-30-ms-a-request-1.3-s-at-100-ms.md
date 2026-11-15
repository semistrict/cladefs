---
id: TASK-449
title: >-
  A restore makes 13 bucket round trips one after another (21 on a cold host):
  0.41 s at 30 ms a request, 1.3 s at 100 ms
status: To Do
assignee: []
created_date: '2026-11-15 12:58'
updated_date: '2026-11-15 14:48'
labels:
  - perf
  - review-2026-11-15
dependencies: []
references:
  - TASK-443.5
  - TASK-443.14
documentation:
  - docs/measurements/macos-bucket-lifecycle-2026-11-15/note.md
priority: high
type: enhancement
ordinal: 849000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Measured by TASK-443.5 (cladefs remote::lifecycle::bucket_lifecycle_by_phase, macOS, a bucket in memory 30 ms a request away; docs/measurements/macos-bucket-lifecycle-2026-11-15). A fork of a point and the fork's first open, the point open on the host: 14 requests in 13 round trips in series, never more than 2 in flight: 414 ms of bucket time at 30 ms a request, 1,325 ms at 100 ms, the same at 1,024, 32,768 and 262,144 extents. The fork: 6 requests, each after the last (child's record GET, create, the reference's create, the point's record GET, the child's record GET again, its update). The open: 8 requests in 7 round trips (child's record and head at once; head again; the base's head; the cell GET; the cell create; the head create; the record again). The child's record is read 4 times and written twice, its head read twice then created, for a branch this host made 0.2 s before. On a host that never saw the point: 25 requests in 21 round trips, 672 ms at 30 ms, 2,138 ms at 100 ms, and the base's checkpoint downloaded (108 KB at 32,768 extents, 856 KB at 262,144); the point's store is opened twice over (ExtentStore::take_with: create_placed opens and drops it, then host_store::take opens it again: 11 requests where the first open's 6 would do). Every restore pays this before its VM boots; nothing in it grows with the base, so it is felt at any size, in proportion to the bucket's distance. Not measured here: the fsyncs of the same path on a shared disk (TASK-443.5, Linux).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A restore on a warm host costs at most half the round trips in series it does today (13), shown by bucket_lifecycle_by_phase and pinned by a test counting its requests
- [ ] #2 A point taken from the bucket is opened once, not twice (cold point-open)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
