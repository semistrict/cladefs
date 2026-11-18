---
id: TASK-633
title: 'Cells across buckets: a bucket per cloud region, forks that span them'
status: To Do
assignee: []
created_date: '2026-11-18 17:02'
labels:
  - bucket
  - multi-region
dependencies: []
references:
  - TASK-537
  - TASK-406
documentation:
  - >-
    backlog/docs/doc-23 -
    Cells-across-buckets-a-bucket-per-cloud-region-forks-that-span-them.md
priority: high
type: feature
ordinal: 931000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
cladefs has one bucket a host and names cells, extents and references by IDs unique only in one bucket, so a fork lives in its parent's bucket. The user chose (2026-11-18) a bucket per cloud region, on S3, GCS and Azure alike, each cell with one home bucket, and forks that span buckets: a child in bucket B forked from a version of a parent in bucket A, keeping its origin and lineage. A one-way flattened copy was rejected. A same-key mirror of the version into B is allowed, so B's children stop depending on A. Cell IDs carry a 16-bit bucket number; bucket numbers come from an operator registry in each host's configuration; hosts and collectors write to every bucket. doc-23 is the design and the order of the subtasks. Not in scope: hosts of several cloud regions over one bucket, and surviving the loss of a home bucket's cloud region.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A child in one bucket is forked from a version of a cell in another bucket, on another cloud, and reads, commits, versions and forks as a same-bucket child does
- [ ] #2 With the parent's bucket out of reach, a child is taken, commits and uploads; after its mirror, it also reads cold and new forks of that version are made in its bucket
- [ ] #3 The collector of either bucket deletes nothing a child in the other needs, as modelled in TLA+ with buckets that go down, with mutations TLC shows breaking it
- [ ] #4 CONTEXT.md, doc-7, doc-16 and docs/operating-a-host.md say how buckets, their numbers, registry, KMSes and mirrors work
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
