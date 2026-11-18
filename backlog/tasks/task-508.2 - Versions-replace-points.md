---
id: TASK-508.2
title: Versions are the only named past states
status: In Progress
assignee: []
created_date: '2026-11-18 00:10'
updated_date: '2026-11-18 02:41'
labels:
  - bucket
  - host
dependencies:
  - TASK-476
documentation:
  - doc-20
parent_task_id: TASK-508
ordinal: 899000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Two mechanisms name a past state today: a version, kept inside its cell (doc-18), and a frozen log of its own that hosts fork (the code's points: Record::point, ForkStore's point flag, SharePoint). doc-20 has forks made only from versions, so the second goes. Making a version must stay local: one commit closing the segment, no bucket round trip.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Forks are made only from versions, on the version's host locally or elsewhere once it is uploaded
- [ ] #2 Dropping a version with children keeps its data until they go
- [ ] #3 Shared versions replace the code's shared points; nothing of the second mechanism is left
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Research 2026-11-18 (what the cut touches):
1. Points are a whole mechanism, not a flag: a point is a frozen branch of its own with a record (record::Kind::Point, Record::point), its own DEK (keys::Ring::for_point), forks of it (branch::fork_point*, resume_point_fork, finish_point_fork, Child.point), sharing (branch::share, Error::Point/NotPoint/NotShared), the collector's point-clone references (TASK-395), the store's point role (State.point, Uploader::is_point, StoreRole::Point), the daemon's kept-open points for zygote restores (cladefsd/stores.rs Point, keep, LINGER, fork_from, at_point), ForkStore's point/shared flags, SharePoint, cladefsctl, and specs/bucket (BucketCellsPoints and the restores of points).
2. Versions already do the bucket's half: branch::fork_version shares fork_with with points (record, reference, check, fork), Head::versions, Writer::share_version, drop_versions, the collector's versions pass, and a store opened as a version's copy (StoreFile.version, Recovering::version). The daemon has no version API at all.
3. A VM snapshot's point crosses into the vendored Firecracker fork: vmm/src/lib.rs calls take_snapshot(point path), and the VMM's persisted device state carries point and point_branch (vendor/firecracker, devices/virtio/extent/persist.rs). Replacing it needs a Firecracker-fork change, a release tag and a submodule bump, not this repo alone.
4. TASK-476 (In Progress, another session, branch t3/fast-cheap-volume-snapshots) is building exactly the daemon half: local-first versions in the source cell for VM snapshots, restores from them, a rolling window of them. Doing it here too would duplicate and conflict.
Order, once TASK-476's daemon versions land:
a. Daemon API: MakeVersion, DropVersions, ShareVersion, ForkStore from a version (ForkStore's point and shared flags go); cladefsctl and Go to match.
b. VM snapshots (with the Firecracker fork): take_snapshot makes a local version of each of the VM's cells; the persisted state names (cell, version); a restore forks the version, a zygote keeping the version's copy open as it keeps a point open today; holds_what checks against the version.
c. Delete the point mechanism: record kind, fork_point*, point DEKs, Child.point, share, the errors, StoreRole::Point and State.point, the kept-open points, the collector's point-only paths, the specs' point configs; tests rewritten to versions.
d. Measure net lines; docs (doc-7 Points, doc-10's generations as shared versions).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-18: not started in code. Blocked by TASK-476's daemon versions (another session, same ground) and by the Firecracker fork's snapshot contract (point path and branch in the VMM's persisted state). Plan recorded.

2026-11-18 (t3/cell-versions 2674fdd85, 258965a41): the daemon's versions, local-first (MakeVersion one local commit; ListVersions, DropVersions, ShareVersion; CreateStore's version makes a copy elsewhere; StatStore says a copy's version); a VM snapshot (quiescent FORK) makes a version and a copy at its path, no upload in its pause; ForkStore's point/shared and SharePoint gone; a version's copy records its source, a daemon fork of it uploads the source first; copies delete nothing in the bucket. Fixed on the way: a fork of a version copy into another cell sealed its checkpoint under the copy's unwritten key (children could not read). Restores of a version cost 9 requests in 8 per fork against a point's 7 in 6 (pinned in lifecycle). Remaining: the bucket's point mechanism itself (Record::point, fork_point*, keys for points, Child.point, branch::share, the collector's point-only paths, specs/bucket's point configs), Firecracker's persisted point/point_branch names (semantics already a version copy), docs.
<!-- SECTION:NOTES:END -->
