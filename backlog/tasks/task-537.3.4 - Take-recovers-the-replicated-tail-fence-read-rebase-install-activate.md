---
id: TASK-537.3.4
title: 'Take recovers the replicated tail: fence, read, rebase, install, activate'
status: Done
assignee:
  - '@claude'
created_date: '2026-11-18T17:02:00Z'
updated_date: '2026-11-18T23:20:00Z'
labels:
  - durability
  - peers
dependencies:
  - TASK-537.3.3
documentation:
  - >-
    backlog/docs/doc-21 -
    Replicated-commits-a-majority-of-hosts-disks-before-the-bucket.md
parent_task_id: TASK-537.3
priority: high
type: feature
ordinal: 934000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-21 step 4. A take on any host must recover every Replicated commit: fence f+1 members other than the old writer, read the longest log, check its MACs, rebase it (Wal::rebase accepting a tail after last_commit), install the new configuration and activate it by the head CAS before serving.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Take fences f+1 non-writer members (the old writers host counted only when it is the taker) and recovers the longest log
- [x] #2 Wal::rebase accepts a log whose first commit follows last_commit
- [x] #3 New configuration installed on f+1 members and activated before the store serves
- [x] #4 Tests: writer host lost, its Replicated commits recovered on another host; a zombie writer refused after the fence
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
verifying-reviewer (9f8ae8dbb): a take with its own WAL ignored members' logs however stale (now members_ahead decides), own extents could trail records (order fixed), an f = 0 take left a stale configuration (now replicates at its f until 537.3.5), promises renamed over (a file per epoch), recovery tests recovered from the bucket (the lost host's runtime now stopped). Blind spot: tests dropped the lost host's uploader, which uploaded on drop; now lose() stops its runtime. Not covered by a test yet: the stale-WAL retake (the reviewer's scratch reproducer) — TASK-537.3.6.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
A take of a replicated cell whose head names a configuration fences f+1 of its members other than its writer at the take's epoch (the writer's own copy counting only on its own host with its WAL), reads the longest copy of each log, trims them against the head's last commit, restores them as the store's WAL if it has none, seeds the new configuration's members and activates it by the head's swap once f of them hold the seed; old members drop what they held. Tested in local_cell with four hosts' replica directories in memory: the bucket behind, a take recovers every commit from the members alone; one member too few to fence refused. Rebase accepts a tail after last_commit through restore's trimming and start file.
<!-- SECTION:FINAL_SUMMARY:END -->
