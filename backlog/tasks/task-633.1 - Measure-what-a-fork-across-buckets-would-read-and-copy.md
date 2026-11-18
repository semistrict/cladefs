---
id: TASK-633.1
title: Measure what a fork across buckets would read and copy
status: To Do
assignee: []
created_date: '2026-11-18 17:02'
labels:
  - bucket
  - multi-region
dependencies: []
documentation:
  - >-
    backlog/docs/doc-23 -
    Cells-across-buckets-a-bucket-per-cloud-region-forks-that-span-them.md
parent_task_id: TASK-633
priority: high
type: spike
ordinal: 932000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Whether a child should read its parent's bucket lazily or have its version mirrored depends on how much of a base a child reads, how much of a version is dead bytes a mirror would copy, and how slow a cold inherited read is a cloud region away (doc-23, Costs). None of it was measured: the only cross-region figure (docs/measurements/gcs-round-trips-2026-11-08.md, ~130 ms a durable commit) had hosts a region from the bucket on an older protocol. Real cloud pairs need real buckets, which the no-GCP rule forbids without the user's word.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Bytes a child reads over its version's object bytes, for at least two real container-image bases under a sandbox workload
- [ ] #2 The dead-byte share of those versions' objects
- [ ] #3 A cold inherited read and a cold restore timed under injected bucket latency (about 70 ms) on ubu1, written up in docs/measurements
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
