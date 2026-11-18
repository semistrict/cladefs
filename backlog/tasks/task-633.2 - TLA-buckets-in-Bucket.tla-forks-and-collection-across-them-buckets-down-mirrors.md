---
id: TASK-633.2
title: >-
  TLA+: buckets in Bucket.tla, forks and collection across them, buckets down,
  mirrors
status: To Do
assignee: []
created_date: '2026-11-18 17:02'
updated_date: '2026-11-18 20:49'
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
type: task
ordinal: 933000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Forks across buckets split today's single-bucket steps between two buckets, either of which may be out of reach, and mirrors move references from one bucket to another (doc-23, What the spec must check). Those are rare interleavings, which the project specs before it builds.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Bucket.tla models cells as <<bucket, id>>, buckets going down and coming back, GcFinish split into its retirement and its removal, and mirrors with their marker
- [ ] #2 NothingNeededGone, ReferencesNamed, ReferencesStand and NoIdTwice hold across buckets; liveness: references retired and mirrors completed once every bucket is up
- [ ] #3 Each new rule has a mutation config that TLC shows breaking an invariant; every model runs in under 10 minutes and scripts/check-tla.sh runs them
- [ ] #4 Forks of a version land at every point of its mirror: before its copy starts, during it, between its last object and its marker, after the marker, and racing each other, the collector of either bucket, and a bucket going down; nothing needed goes and every child reads its version whole
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
