---
id: TASK-443.4
title: >-
  Investigate: every sandbox ever created costs every collector sweep ~5
  requests for ever, and a popular base's pass lists every retired mark
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 12:24'
updated_date: '2026-11-15 14:48'
labels:
  - perf
  - review-2026-11-15
dependencies: []
documentation:
  - docs/reviews/2026-11-15-sandbox-performance.md
parent_task_id: TASK-443
priority: high
ordinal: 838000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/extent_remote/src/collector.rs: a dead cell keeps its cell object, record (records_kept), head (Kind::Head is Fate::Stays), gc/lease and a retired/ mark under its parent's cell; each sweep takes its lease, lists it and releases it (2 GET, 2 PUT, 1 LIST). At 10^5 sandboxes a day, after 30 days a sweep is ~1.5x10^7 requests and hours long, and garbage lingers as long. passed::list lists the whole cell prefix before the skip decision, references and marks included: a base cloned 10,000 times a day holds 10^6 marks after 100 days. The request counters count a multi-page LIST as one. Found by reading the code, not measured (docs/reviews/2026-11-15-sandbox-performance.md).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 scripts/gc-sweep.sh over buckets of N cells all deleted (N at three sizes), three sweeps each: the third sweep's requests by kind and wall time against N; and one pass over a point with N clones made and deleted, its LIST pages and peak memory
- [x] #2 The notes give the measured cost and the workload size at which a customer would feel it; a fix, if needed, is filed as its own task
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. cladefs-gc-fill gets a mode, --clones: per tenant a base cell (one branch of SEGMENTS segments), a point of it in a cell of its own, and CELLS sandboxes, each a cell with one branch cloned from the point (branch::fork_point, as hosts make them), SEGMENTS segments written, checkpointed, then deleted (branch::delete). A unit test fills a small one and sweeps it, asserting the third sweep's requests by kind, cells skipped and objects left exactly, and that a clone made and deleted after stops the base's cell skipping.
2. scripts/gc-sweep.sh: any number of sweeps (CLADEFS_GC_SWEEPS), the fill's extra flags (CLADEFS_GC_FILL), a table of each sweep's requests by kind, cells skipped and objects left in the bucket, and CLADEFS_GC_CELL: after the sweeps, a pass over that cell alone in a process of its own, once with the sweeps' options (does it skip) and once with another grace (a pass in full, as while clones churn): requests, objects under the cell, LIST pages (objects/1000), peak RSS, seconds.
3. Run on this Mac, file buckets, release build: 1 tenant, N = 100, 1,000, 10,000 sandboxes (100,000 if a run stays under 5 min), 3 sweeps each, CLADEFS_GC_CELL=1/1. Fit requests a dead cell a sweep.
4. Results under docs/measurements/macos-gc-dead-cells-2026-11-15 with a note.md; task notes with numbers, the extrapolation to 1e5 sandboxes a day for 30 days (labelled), and a verdict; one fix task if real.
5. cargo clippy, nextest of extent_remote, scripts/check-exclusions.py, scripts/upstream-changes.py --check; commit; push to t443-4.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Measured 2026-11-15 on this Mac (Apple M5 Pro, file buckets on APFS, release build, 16 passes at once, one run each): docs/measurements/macos-gc-dead-cells-2026-11-15 (note.md has the tables and commands).

Verdict: real, both halves. Fix filed: TASK-455.

Commands (CARGO_TARGET_DIR set; n = 100, 1000, 10000):
FXDAX_GC_TENANTS=1 FXDAX_GC_CELLS=$n FXDAX_GC_SEGMENTS=4 FXDAX_GC_LARGEST=16 FXDAX_GC_FILL='--deleted' FXDAX_GC_AT_ONCE=16 FXDAX_GC_SWEEPS=3 scripts/gc-sweep.sh WORK/deleted-$n RESULTS/deleted-$n
FXDAX_GC_TENANTS=1 FXDAX_GC_CELLS=$n FXDAX_GC_SEGMENTS=4 FXDAX_GC_LARGEST=16 FXDAX_GC_FILL='--clones 1024' FXDAX_GC_AT_ONCE=16 FXDAX_GC_SWEEPS=3 FXDAX_GC_CELL=1/1 scripts/gc-sweep.sh WORK/clones-$n RESULTS/clones-$n
fxdax-gc-fill D 1 3 4 16 --clones 1024; fxdax-gc file://D/ --grace 0 (three times); fxdax-gc file://D/ --usage

(a) Third sweep, every cell skipped. Deleted roots, N = 100 / 1,000 / 10,000: GET 200 / 2,000 / 20,000; PUT (update) 200 / 2,000 / 20,000; LIST 100 / 1,000 / 10,000; delimiter LIST 2 / 2 / 11; total 502 / 5,002 / 50,011; 1.2 / 9.7 / 110.2 s; objects left 400 / 4,000 / 40,000. Sandboxes cloned from a point, deleted (N + base + point cells): total 512 / 5,013 / 50,022; 1.0 / 12.6 / 188.9 s; objects left 525 / 5,025 / 50,025. Fit, exact at every size: 5 requests a dead cell a sweep (2 GET + 2 PUT of gc/lease, 1 LIST) + 1 delimiter page per 1,000 cells. A dead cell keeps 4 objects, 422 bytes (cell 46, record 67, head 73, gc/lease 236), and a clone a fifth, its 4-byte retired mark under the base's cell. The sweep's peak RSS 12-22 MiB, flat. Seconds a cell (10-19 ms at 16 at once) are the file bucket's fsyncs, not S3's.

(b) Pass over the base's cell after N clones made and deleted, N = 100 / 1,000 / 10,000: objects listed 122 / 1,022 / 10,022 (N of them marks); LIST pages 1 / 2 / 11 (objects/1,000; the counter says 1); unchanged it skips, 5 requests, 0.018 / 0.078 / 0.261 s, peak RSS 10 / 10.5 / 19 MiB; in full (as whenever a clone was made or retired since) 33 requests, 0.054 / 0.222 / 0.283 s, 31 / 32 / 39 MiB. About 1 KiB of RSS a mark listed. The claim 'never skips while clones churn' holds (fxdax-gc-fill's test asserts a sandbox made and deleted puts the base's pass in full for two sweeps); the review's 'holds every reference' does not: live references are few, the marks are what grows.

Extrapolation (not measured), 10^5 sandboxes a day for 30 days = 3x10^6 dead cells: a sweep = 5 x 3x10^6 = 1.5x10^7 requests (6x10^6 GET, 6x10^6 PUT, 3x10^6 LIST) + 3,000 delimiter pages; 1.5x10^7 objects kept (3x10^6 x 5), ~1.3 GB. Time at 16 at once on a bucket 30 ms away: 5 serial requests = 150 ms a cell / 16 = 9.4 ms a cell x 3x10^6 = 7.8 h (2 h at 64 at once); the Mac's file bucket at its measured 11 ms a cell would take 9 h. Cost at S3 Standard list prices as I remember them, unverified (PUT/LIST USD 0.005 per 1,000, GET 0.0004 per 1,000): 9x10^6 x 0.005/1,000 + 6x10^6 x 0.0004/1,000 = USD 47 a sweep; swept hourly, ~USD 1,100 a day on day 30, rising ~USD 38 a day each day. One base taking all 10^5 clones a day: 3x10^6 marks, 3,000 LIST pages a pass (~2.5 min at 50 ms a page, serial) and ~3 GiB of RSS, every sweep; the review's 10^4 a day for 100 days: 10^6 marks, 1,000 pages, ~1 GiB.

When a customer feels it: a sweep passes an hour at about 4x10^5 dead cells (4 days at 10^5 a day, 40 days at 10^4 a day) and USD 1 at about 6x10^4; at 10^3 sandboxes a day it is 1.5x10^5 requests and under a minute at 16 at once after 30 days, not felt for a year. So: not a problem for a small deployment, a growing bill and garbage lingering for hours from 10^4 sandboxes a day within weeks.

Also seen: a dead cell is passed in full twice before it skips (the second pass re-creates its marks: 10,000 creates in sweep 2). In TASK-455's description.

Added: fxdax-gc-fill --deleted and --clones EXTENTS (with a parser and two tests, one sweeping what they fill and asserting the requests, skips and objects left exactly); scripts/gc-sweep.sh FXDAX_GC_FILL, FXDAX_GC_CELL, any FXDAX_GC_SWEEPS, gc-sweep-requests.md and gc-sweep-cell.md.

Checks: cargo nextest run -p extent_remote 316 passed (macOS); cargo clippy -p extent_remote --all-targets: nothing in the changed file; scripts/check-exclusions.py and scripts/upstream-changes.py --check pass; scripts/tests -k gc_sweep pass; shellcheck scripts/gc-sweep.sh clean.

Not done: 100,000 cells (a fill and sweep each past 5 minutes); a real bucket; the workspace's nextest in full on macOS and any Linux run (no ubu1 in this task): DoD #1 open, task left In Progress.
<!-- SECTION:NOTES:END -->
