---
id: TASK-633.7
title: 'Forks across buckets: ForkCell into another bucket, collection across buckets'
status: To Do
assignee: []
created_date: '2026-11-18 17:03'
labels:
  - bucket
  - multi-region
dependencies:
  - TASK-633.5
  - TASK-633.6
documentation:
  - >-
    backlog/docs/doc-23 -
    Cells-across-buckets-a-bucket-per-cloud-region-forks-that-span-them.md
parent_task_id: TASK-633
priority: high
type: feature
ordinal: 938000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
With unique IDs, routing and takes that ask only their own bucket in place, a fork can make its child in another bucket: its head in B, its reference and the checkpoint at the version in A, ordered as today, and B's pass retires the reference in A (doc-23, A fork across buckets). The user allowed every host and collector to write every bucket.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 ForkCell (and the VM snapshot restore path) takes the child's bucket; the child's head is in it, its reference and checkpoint in the parent's
- [ ] #2 A fork that crashes at any step resumes or is collected, as in one bucket
- [ ] #3 A pass that cannot reach a bucket it needs deletes nothing that depends on it and records no listing digest
- [ ] #4 bucket_cell's simulation runs with two buckets, one of them failing at times, and finds nothing needed gone
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
