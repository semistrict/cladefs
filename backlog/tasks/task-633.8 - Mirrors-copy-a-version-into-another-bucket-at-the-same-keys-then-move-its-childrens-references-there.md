---
id: TASK-633.8
title: >-
  Mirrors: copy a version into another bucket at the same keys, then move its
  children's references there
status: To Do
assignee: []
created_date: '2026-11-18 17:04'
labels:
  - bucket
  - multi-region
dependencies:
  - TASK-633.7
documentation:
  - >-
    backlog/docs/doc-23 -
    Cells-across-buckets-a-bucket-per-cloud-region-forks-that-span-them.md
parent_task_id: TASK-633
priority: medium
type: feature
ordinal: 939000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Until its version is copied into its own bucket, a child across buckets reads its parent's bucket cold and pays egress, and new forks need that bucket up. The user accepted a same-key, byte-for-byte mirror (not a flattened copy) made in the background (doc-23, Mirrors). Copies are authentic where they land, keys are written once.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A control call mirrors a version into a bucket: every object of its checkpoint and their moves copied (server-side where the clouds allow: S3 CopyObject, GCS rewrite, Azure Put Blob From URL; GET and PUT otherwise), then the marker
- [ ] #2 Children's references move to the mirror's bucket in the order the spec checks; reads of the version come from the mirror once its marker exists
- [ ] #3 New forks of a mirrored version into its bucket reference the mirror alone; the mirror goes when no reference to it stands
- [ ] #4 Tests: a child reads cold and forks with its parent's bucket failing after its mirror; a crash at each step of a mirror leaves nothing needed gone
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
