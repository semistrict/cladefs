---
id: TASK-653
title: >-
  Collector pass fails with Lost on a parent's checkpoint at an origin that a
  fork has not written yet (sim seeds bases 6496, regions 2325)
status: In Progress
assignee: []
created_date: '2026-11-19 03:17'
updated_date: '2026-11-19 03:32'
labels:
  - collector
  - sim
dependencies: []
priority: medium
type: bug
ordinal: 1034000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found 2026-11-18 by soaking the simulation (SIM_SOAK=40, 8000 seeds): a collector pass ends with Error::Lost naming a parent's checkpoint at an origin, e.g. "collector: lost: tenants/1/cells/2/checkpoints/1.0" (profile deleted_bases, seed 6496) and "collector: lost: tenants/1/cells/0008000000002/checkpoints/1.3" (profile cells_across_regions, seed 2325). The checkpoint is never deleted: the pass GETs it (NotFound) before the fork writing it (lifecycle::record_fork / fork_version) creates it; the error has no "which X lives on" suffix, so it comes from recover_live/lost_is_ok returning false or keep() with KeptBy other than Referred. Rare in one bucket (1 seed in ~8000); ~3x more frequent with regions (cross-region delays and region outages widen the window between a child's head and its parent's checkpoint at the origin).
Repro: SIM_PROFILE=bases SIM_SEED=6496 (or SIM_PROFILE=regions SIM_SEED=2325, on the branch with TASK-633.10) cargo nextest run --cargo-profile sim -p local_cell --run-ignored only sim::one; SIM_TRACE=1 SIM_JOURNAL=1 for the trace.
A pass erroring deletes nothing (safe) but the violation is a real protocol gap: lost_is_ok should treat a not-yet-written origin checkpoint of a fork being made as decided by the next pass.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Seeds 6496 (bases) and 2325 (regions) pass, with a regression test pinning the race
- [ ] #2 The rule that handles it is shown needed by a seed search
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Find which Lost exit fires (tagged temporarily: recover_live -> lost_is_ok false); pass the record the pass read into lost_is_ok; a fork being made then and made now is a stale read: unsettled, next pass decides (also for abandon finding it made). Pin both seeds in their profiles; soak both profiles.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Confirmed pre-existing: seed 6496 (bases) fails at 4784e3c05 and at 82bb2aa37. Cause: recover_live read a restore's record while forking; its origin checkpoint (written by the restore after its reference, before it is made) was not there yet; lost_is_ok read the record again, now made (forking None), and returned false -> Error::Lost. Fixed in d1f84648d (branch, verification pending): lost_is_ok(bucket, cell, read, writes) returns true (decided by the next pass) when the pass's read was forking and the record is made now, and when abandon finds it made. Seeds pinned (test_holds found lists); bases and regions profiles hold over 8000 seeds each (SIM_SOAK=40). bucket_cell 334/334, sim 44/44.
<!-- SECTION:NOTES:END -->
