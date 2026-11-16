---
id: TASK-462
title: >-
  A collector pass calls a fork point lost when a takeover's writer forks again
  at the same segment number
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 14:53'
updated_date: '2026-11-15 23:29'
labels:
  - bug
  - collector
  - sim
dependencies: []
ordinal: 830000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-411.28's run of extent_store's simulation (Mac): sim::restores_taking_forks_ids fails at seed 180 of profile taking_ids ('collector: lost: segment 1 of 1/2/2, which 1/76/76 lives on'); reproduce with SIM_SEED=180 SIM_PROFILE=taking_ids cargo nextest run -p extent_store --run-ignored only -E 'test(=sim::one)' on branch t3/require-sealed-storage-keys. Seeds 0-179 and 181-1199 of that profile pass there; the base (a9d258064) passes 200-1199 and its suite's 0-199, its request schedule differing (every store sealed adds requests), so whether this is new or only exposed is not known. What happens: writer 1 of branch 2 forked children (3d, 27, 14, 7a) at its segment 1, never acknowledged; writer 2 took over at the head naming segment 0 alone (end 1) and later forked 76 at its own segment 1 (2.1). A pass of cell 2 listing references finds fork point (2, 1) (the old children's), reads the head while it still names only segment 0, so recover_at's state_at finds the segment unnamed (Error::Lost, recovery.rs 'segment {at:x} of {branch}'); keep_points then asks needing(), which by then reads 76's reference and record (origin 2 at 1, live) and stops the pass reporting 76's fork point lost. References name their fork point by segment number alone, not the epoch of the segment's writer, so a fork point a takeover dropped and a later one at the same number are one key. The pass stops rather than deletes, but the sim counts the error a violation.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Seed 180 of taking_ids passes, by a fix that tells a fork point a takeover dropped from a later fork at the same segment number (or reads the head after the references and records it judges by)
- [x] #2 A direct extent_remote test of the race fails without the fix
- [x] #3 sim::restores_taking_forks_ids passes; the sim part of scripts/test.sh passes
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. In collector::keep_points, when a referenced fork point's state is lost and needing() finds a child living on it, recover the state again (after the child's record was read): its fork checkpointed the state before referring (branch::record_fork), so it is found, listing anew (Checkpoints serves each fork point from the pass's listing once). Only a second loss stops the pass.
2. A direct test in collector.rs driving a pass on SimBucket, pausing it before needing()'s listing while the writer forks a child at the point; failing without the fix.
3. Seed 180 of taking_ids, the sim part, extent_remote's tests on macOS; workspace and sim on ubu1.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-15 (on t3/explore-tenant-encryption-keys, after TASK-411.28 merged): fixed in collector::keep_points by recovering a lost referenced fork point again once a living child is found. Test passes_finding_a_fork_point_lost_then_a_child_forked_there_recover_it_again fails without it (Err(Lost("..., which 1/2/2 lives on"))). macOS: SIM_SEED=180 SIM_PROFILE=taking_ids sim::one passes; scripts/test.sh sim part 50/50; extent_remote 327/327. ubu1's sim part on 9fc2eaa46 (before the fix) failed only restores_taking_forks_ids at seed 180. Linux workspace and sim on the fix pending.
<!-- SECTION:NOTES:END -->
