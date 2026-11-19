---
id: TASK-633.8
title: >-
  Mirrors: a version forked into another bucket is copied there in the
  background, its children switched once the copy is whole
status: In Progress
assignee: []
created_date: '2026-11-18 17:04'
updated_date: '2026-11-19 08:26'
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

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Order (each its own verified commit):
1. Layout: the mirror marker (create-only, under P's prefix, naming at and the checkpoint epoch) and the mirror's pin (a reference of its own under P's prefix); Kinds parsed, kept by no pass but the foreign-cell rule.
2. B's collector over a foreign cell's prefix (P numbers another bucket): never a dead cell; keeps a copy while P's head in A names its version or a reference in B stands, else deletes the marker first, then the objects (copiesKeptWhileNamed, copyRelists, markerGoesFirst). Before anything is ever written under a foreign prefix.
3. Reads: a key of a foreign cell read from the own bucket first when it holds a marker of that cell (keys written once: a copy equals its source), else the cell's bucket.
4. The mirror job: pin, head read in A, copy (GET+PUT create-only; server-side later), marker, list A refs after the marker and refer in B, retire+delete in A, drop pin; resumes from its pin, after the marker goes on to the listing.
5. Forks: reference in B when marked, else A; read again after referring in A (forksRecheck, forksLeaveA); the first fork into B starts the mirror in the background.
6. Tests per AC #4, and the simulation profile with mirrors (TASK-633.10).
<!-- SECTION:PLAN:END -->
