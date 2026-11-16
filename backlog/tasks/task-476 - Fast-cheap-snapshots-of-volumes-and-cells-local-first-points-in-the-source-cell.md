---
id: TASK-476
title: >-
  Fast, cheap snapshots of volumes and cells: local-first points in the source
  cell
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-16 02:25'
updated_date: '2026-11-16 15:52'
labels:
  - snapshot
  - bucket
  - host
dependencies:
  - TASK-475
documentation:
  - doc-18
priority: high
ordinal: 881000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The user wants extremely fast and cheap snapshots at volume and cell level (2026-11-15). Today a VM snapshot pauses the VM while the store's whole upload backlog uploads (fork_at_rest -> Store::commit_fork -> Remote::forked waits for the fork epoch's acknowledgement, in order, up to the 1 GiB lag), then makes 20 requests in 18 serial round trips and writes a whole-state checkpoint twice (TASK-452), because each snapshot is a fork into a new cell. Decided with the user that day: (1) local-first: a snapshot returns once its fork commit is durable on local disk; the point is pending until acknowledged, its record made by the uploader then; same-host restores work at once, other hosts wait for durable; (2) points live in the source's cell, as versions, not a new cell: no cross-cell checkpoint or reference at snapshot time, the first restore into another cell pays the fork-point checkpoint once; (3) whole-cell cuts in one process, cladefsd (TASK-475), the holder taking every volume's commit lock in order, one object naming each volume's (branch, at) as the atomic commit; (4) scale: frequent snapshots, few kept (a rolling window), so taking and deleting a point must be cheap and GC must reclaim promptly. Related: TASK-452, TASK-364.1, TASK-242, TASK-456.4.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Design written into a backlog doc and doc-7, checked by the bucket simulation and specs/bucket before coding
- [ ] #2 A volume snapshot pauses its VM for no bucket round trip: its pause is the freeze and one local fsync, measured on ubu1
- [ ] #3 A snapshot makes O(1) bucket requests and bytes not growing with the extents (bucket_lifecycle_by_phase)
- [ ] #4 A whole-cell snapshot cuts every volume at one moment, shown by a test committing to two volumes concurrently
- [ ] #5 The daemon lists the volumes it holds of a cell, VM-served and mounted alike, each with a handle that pauses its commits (moved from TASK-475.3)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Design: doc-18 (versions named by the volume's head, local-first cuts, restores, collection, whole-cell cuts), doc-7's Versions section.
2. specs/bucket: versions in heads, DropVersion, restores of versions, the collector's keeping; rules versionsKept, versionsCarried, versionsAcked with mutations; BucketVersions and BucketCellsVersions models under 10 min; every existing model and mutation still passes.
3. extent_frame/extent_remote: footer and head versions, FORMAT 14; writer ack/takeover/deletion/drop; restores; collector; Rules fields, simulation tests, hand mutants.
4. extent_store: commit_version, versions in the log and rebase, pending versions.
5. cladefs: SNAPSHOT request, device snapshot, control API (Snapshot, ListVersions, WaitVersion, DeleteVersion, ShareVersion, RestoreVersion), cladefsctl, Go; lifecycle measurement and pinned request counts (AC #3); pause measured on ubu1 (AC #2).
6. cladefsd: a cell's volumes with pause handles (AC #5); SnapshotCell and the cell object's cuts; concurrent two-volume cut test (AC #4).
7. Full macOS + Linux (ubu1, fxjob) runs; upstream-changes check.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-16: the user chose versions named by the volume's head over a record per snapshot (records stay for good; every pass would read each one ever made). Design: doc-18; doc-7's Versions section. specs/bucket: head vers, commit "version", DropVersion, StartRestoreVersion, the collector keeping each version's state; rules versionsKept, versionsCarried, versionsAcked; models BucketVersions (1,988,678 states, 56 s), BucketVersionsTakeovers (1,620,852, 34 s), BucketCellsVersions (499,990, 18 s); mutations NoVersionsKept, NoVersionsCarried, NoVersionsAcked, NoForksCheckVersions, NoClonesReferFirstVersions, NoCollectorAbandonsVersions each find their violation; every other bucket model and mutation still passes (73 in all, Mac).
Code (slice 1, bucket and store): extent_frame Contents::versions/VersionCut; extent_remote FORMAT 14, Head::versions, Writer naming versions at acknowledgement, drop_versions/share_version, takeovers carrying them, deletions ending them, record::Forked, branch::fork_version(_with), the collector keeping versions' states and abandoning restores of dropped versions, recovery::recover_version, rules versions_kept/carried/acked; extent_store format 12, Store::commit_version/pending_versions, version copies (StoreFile::version), Remote::versions/drop_versions/share_version. The simulation's versions profile holds over 200 seeds after it found a restore into another cell, being made, unrecoverable until its checkpoint lands (passes now skip it: collector::lost_is_ok); each rule's sim test finds its violation; hand mutant versions-named-past-their-cuts killed. extent_frame/remote/store suites pass (623); workspace clippy passes.

Slice 1 (bucket and store) on main as 48b8eeb04, rebased onto TASK-474's renames (extent_bucket; vdisks; local/uploaded; store format 13, bucket format 14). Verified at 3e8b3d25f: macOS clippy, 2,558 workspace tests, simulation, Shuttle, 516 script tests, Go; Linux (ubu1, fxjob) clippy, 2,868 workspace tests, simulation, Shuttle; all 73 bucket TLA+ models and mutations. After rebasing onto 3e0256500 (pages.rs, memory.rs only): clippy and extent_store's 251 pass. Next: cladefs API (ExtentStore::cut_version, control API Snapshot/ListVersions/WaitVersion/DeleteVersion/ShareVersion, version copies through CreateStore, cladefsctl, Go), the VM process's SNAPSHOT and the device's snapshot, lifecycle request counts (AC #3), the pause on ubu1 (AC #2), then the cell's vdisks with pause handles and SnapshotCell (AC #4, #5).
<!-- SECTION:NOTES:END -->
