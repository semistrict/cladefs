---
id: TASK-633.2
title: 'TLA+: forks across buckets and their automatic mirrors (specs/buckets)'
status: Done
assignee:
  - '@claude'
created_date: '2026-11-18 17:02'
updated_date: '2026-11-18 23:24'
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
- [x] #1 specs/buckets/Buckets.tla, cut down from specs/bucket as specs/replication was, models a parent in one bucket and children in another, references, both buckets' collectors, buckets going down and coming back, and the automatic mirror with its pin, marker and restarts
- [x] #2 NothingNeededGone, MarkersWhole, ReferencesNamed, ChildrenReferred and MirroredChildrenOnB hold; liveness: every mirror begun ends, buckets down and back, under fairness
- [x] #3 Each rule has a mutation config that TLC shows breaking an invariant, and Reaches* models show the spec reaches what it checks; every model runs in under 10 minutes and scripts/check-tla.sh runs them
- [x] #4 Forks of a version land at every point of its mirror: before its copy starts, during it, between its last object and its marker, after the marker, and racing each other, the collector of either bucket, and a bucket going down; nothing needed goes and every child reads its version whole
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. A module of its own, cut down from specs/bucket (forks, references, collection) to one parent in A and children in B, rather than buckets added to Bucket.tla, whose write path would multiply the states past any model under 10 minutes.
2. Forks, the automatic mirror, three collectors, buckets down; rules as toggles, one mutation each; Reaches* models.
3. Safety models without buckets down (a step a bucket down refuses changes nothing); BucketsLive with them, for liveness.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Not checked: a liveness property that a deleted child's references are eventually retired: collector passes are bounded (MaxPasses), so fairness cannot promise one runs; MirrorsEnd is checked. Not modelled: the registry (TASK-633.9's tests), compaction and moves, the children's own data. DoD #1 (cargo suites) is not this task's: it changes no Rust.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
specs/buckets/Buckets.tla checks forks across buckets and the automatic mirror: 11 rules each with a killing mutation, 3 reachability models, and four models (Buckets 74M states 214 s, BucketsVersions 75M 250 s, BucketsThree 11M 36 s, BucketsLive 1.9M 83 s with buckets down and back). TLC found two holes in the first design (mirrorChecks, mirrorResumesMarked) and two unneeded steps; forks read the parent's head, so a fork into B waits for A. doc-23 records the protocol as checked.
<!-- SECTION:FINAL_SUMMARY:END -->
