---
id: TASK-443.4
title: >-
  Investigate: every sandbox ever created costs every collector sweep ~5
  requests for ever, and a popular base's pass lists every retired mark
status: To Do
assignee: []
created_date: '2026-11-15 12:24'
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
- [ ] #1 scripts/gc-sweep.sh over buckets of N cells all deleted (N at three sizes), three sweeps each: the third sweep's requests by kind and wall time against N; and one pass over a point with N clones made and deleted, its LIST pages and peak memory
- [ ] #2 The notes give the measured cost and the workload size at which a customer would feel it; a fix, if needed, is filed as its own task
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
