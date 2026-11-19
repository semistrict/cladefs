---
id: TASK-633.8
title: >-
  Mirrors: a version forked into another bucket is copied there in the
  background, its children switched once the copy is whole
status: In Progress
assignee: []
created_date: '2026-11-18 17:04'
updated_date: '2026-11-19 09:34'
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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Steps 1-4 (dcd176354, e7bdb0714, 6792b32ee, a17a5a0d4): layout (FORMAT 22: pin, marker), B's collector over a copy (collect_copy), reads served from a marked copy first, the mirror job.

Step 5: forks refer through Bucket::mirrors (mirror::Here): in B when the marker is there, else in A; having referred in A, the marker read again after P's head, and if there, the reference written in B and retired in A (forksRecheck, forksLeaveA). Uploader::forked starts ObjectBucket::mirror_wanted on the bucket runtime; a process runs one mirror of a version at a time.

Pins (beyond the spec, which models one mirror): the pin's bytes are its mirror's own; another's standing, a mirror does nothing (UnderWay). A pin unrefreshed for PIN_STALE (10 min) is stale: the mirror refreshes it each minute and right before its marker by conditional write, giving up if it is no longer its own; a stale pin keeps nothing, the next mirror takes it over; B's collector begins a stale-pinned mirror again. Lease assumption documented in doc-23 (As built).

Simplifications (doc-23): GET+PUT copies only; only P's own prefix copied; a cell's whole copy kept while any version needs it.

Tests: forks_refer_where_their_versions_are_mirrored, mirrors_hold_their_pins_alone (bucket_cell), versions_forked_on_a_host_of_another_bucket_make_children_there (cladefs, the fork starting the mirror). Hand mutants, each failing a test: no recheck, no retirement in A, never referring in B, shared pins, refresh unchecked, stale pins keeping, never stale, the fork starting no mirror.
<!-- SECTION:NOTES:END -->
