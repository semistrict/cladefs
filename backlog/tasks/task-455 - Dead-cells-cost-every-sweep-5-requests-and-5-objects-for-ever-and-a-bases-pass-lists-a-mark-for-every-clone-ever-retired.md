---
id: TASK-455
title: >-
  Dead cells cost every sweep 5 requests and 5 objects for ever, and a base's
  pass lists a mark for every clone ever retired
status: To Do
assignee: []
created_date: '2026-11-15 13:16'
updated_date: '2026-11-15 14:48'
labels:
  - perf
  - review-2026-11-15
dependencies: []
documentation:
  - docs/measurements/macos-gc-dead-cells-2026-11-15/note.md
  - docs/reviews/2026-11-15-sandbox-performance.md
priority: high
type: bug
ordinal: 849000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Measured by TASK-443.4 (docs/measurements/macos-gc-dead-cells-2026-11-15, file buckets on macOS, 100 to 10,000 cells, exact at every size). (a) A cell whose branches are all deleted keeps 4 objects for good (cell object, record, head, gc/lease: 422 bytes), plus a retired/ mark under its parent's cell if it was a clone, and every later sweep spends 5 requests on it: 2 GETs and 2 conditional PUTs of its lease and 1 LIST of its prefix, skipping it. A sweep's requests are 5 x every cell ever made. Extrapolated, not measured: at 10^5 sandboxes a day, after 30 days a sweep is 1.5x10^7 requests (6x10^6 GET, 6x10^6 PUT, 3x10^6 LIST), some hours at 16 passes at once on a bucket 30 ms away, and about USD 47 a sweep at S3's list prices as remembered (unverified), growing by a day's sandboxes every day; garbage waits as long as a sweep takes. At 10^3 a day it is 1.5x10^5 requests after 30 days: not felt. (b) The pass over a cell lists its whole prefix before deciding to skip (passed::list), retired marks included: a base whose point was cloned N times lists N marks every pass, N/1,000 LIST pages (counted as 1 request), about 1 KiB of memory a mark (19 MiB at 10,000; about 1 GiB at 10^6, extrapolated), and it skips only if no clone was made or retired since its last pass, so a base in use goes in full every sweep. Also seen: a dead cell is passed in full twice before it skips, the second pass creating its marks again. The rules that keep these objects (records kept, TASK-174; heads final, TASK-407; marks kept for good, TASK-395.13/.14; the lease) each have a safety reason, in OPEN_QUESTIONS.md's decisions on the per-cell collector and specs/bucket: a fix has to keep them or replace them in the spec.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A sweep's requests do not grow with the cells whose branches were all deleted before its previous sweep: scripts/gc-sweep.sh with CLADEFS_GC_FILL=--deleted at 100, 1,000 and 10,000 cells shows the third sweep's requests the same, or bounded by a stated constant, at each
- [ ] #2 The objects a deleted sandbox leaves in the bucket for good, its mark under its parent's cell among them, are none or bounded by a stated constant however many sandboxes were made; the same runs' objects left show it
- [ ] #3 A pass over a base's cell lists and holds a number of objects that does not grow with the clones made and deleted before its last pass: CLADEFS_GC_FILL='--clones 1024' CLADEFS_GC_CELL=1/1 at the three sizes shows its objects listed, LIST pages and peak memory flat
- [ ] #4 A LIST of many pages is counted as its pages in cladefs-gc's request counts
- [ ] #5 specs/bucket checks whatever replaces the rules that keep records, heads, leases and marks, with a mutation each; cladefs-gc-fill's test dead_cells_keep_four_objects_and_cost_a_sweep_five_requests is changed to the new numbers
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
