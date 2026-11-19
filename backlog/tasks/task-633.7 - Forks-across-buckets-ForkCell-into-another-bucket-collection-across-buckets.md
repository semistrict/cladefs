---
id: TASK-633.7
title: 'Forks across buckets: ForkCell into another bucket, collection across buckets'
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18 17:03'
updated_date: '2026-11-19 00:52'
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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-18 (unattended night, my call): a child lives in the forking host's bucket. A host opens the version through its own router (the parent's keys routed to the parent's bucket) and allocates the child's ID from its own bucket, so ForkCell needs no bucket field: a VM manager forks into a region by asking a host there. Shown by versions_forked_on_a_host_of_another_bucket_make_children_there (child ID of B, head in B, reference in A, child reads its parent's data; a mutant without routing fails it with ENOENT). cladefs-gc routes (f847bc4ab), so a child's pass retires its reference in the parent's bucket (children_in_one_bucket_read_their_parents_in_another). Left: AC #1's wording (no bucket field), AC #3 (a pass that cannot reach a bucket deletes nothing and records no digest), AC #4 (sim with two buckets, one failing).
<!-- SECTION:NOTES:END -->
