---
id: TASK-633.8
title: >-
  Mirrors: a version forked into another bucket is copied there in the
  background, its children switched once the copy is whole
status: To Do
assignee: []
created_date: '2026-11-18 17:04'
updated_date: '2026-11-18 20:48'
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
Until its version is in its own bucket, a child across buckets reads its parent's bucket cold, pays egress, and needs that bucket up for those reads. The user decided (2026-11-18): the first fork of a version into another bucket starts copying it there in the background, byte for byte at the same keys (not a flattened copy), one mirror per version and bucket, shared by its children there; children read the parent's bucket lazily until the mirror's create-only marker lands, then their own (doc-23, Mirrors).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The first fork of a version into another bucket starts its mirror there: every object of its checkpoint and their moves copied (server-side where the clouds allow, GET and PUT otherwise), then the marker; later forks find it under way or done
- [ ] #2 Children's references move to the mirror's bucket in the order the spec checks; reads of the version come from the mirror once its marker exists
- [ ] #3 A mirror interrupted by a crash or a bucket down resumes; the mirror goes when no reference to it stands
- [ ] #4 Tests: a child reads cold and forks with its parent's bucket failing after its mirror; a crash at each step of a mirror leaves nothing needed gone
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
