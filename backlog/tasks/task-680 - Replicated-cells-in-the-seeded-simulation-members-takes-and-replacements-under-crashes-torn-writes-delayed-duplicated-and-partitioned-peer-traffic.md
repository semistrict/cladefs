---
id: TASK-680
title: >-
  Replicated cells in the seeded simulation: members, takes and replacements
  under crashes, torn writes, delayed, duplicated and partitioned peer traffic
status: Done
assignee:
  - '@claude'
created_date: '2026-11-20 00:59'
updated_date: '2026-11-20 04:25'
labels:
  - durability
  - peers
  - testing
dependencies: []
references:
  - crates/local_cell/src/sim/world.rs
  - crates/local_cell/src/mem.rs
  - specs/replication/Replication.tla
priority: high
type: task
ordinal: 1061000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found 2026-11-19 comparing cladefs's replication coverage with FoundationDB's simulation (fdbrpc/sim2, AsyncFileNonDurable, the Attrition/RandomClogging/Rollback/Swizzle workloads, restarting tests). local_cell's seeded simulation (src/sim/world.rs) opens every cell with replication: None, so replicated commits are covered only by TLA+ (specs/replication, which abstracts members' logs into one sequence and sees no code) and a handful of hand-written scenarios (tests/replicated.rs, cladefs tests/replicated.rs). TASK-678, a lost commit told Replicated, was found by reading, not by any test; a randomized world with retakes on the writer's own disk and a member out of reach would have found it. FDB's fault classes the hand-written tests don't reach: a crash during or right after a take (restore half written, activation half done), repeated takes in a row, a member reachable by the writer but not the taker (asymmetric partitions that move), RPCs delayed past a later take or delivered twice (a zombie's drop or append arriving after a fence), a member's disk failing mid-append (EIO, ENOSPC) or hanging, and unsynced blocks coming back corrupt rather than missing (MemDisk::crash keeps or drops 512 B blocks; FDB also corrupts and tears sectors).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The simulation runs replicated cells (f = 1 and 2) beside unreplicated ones: hosts with replica directories, members reached through a seeded link that delays, drops, duplicates and partitions (one-way too), and takes that fence, recover and activate
- [x] #2 Its checker holds every commit a guest was told Replicated or Uploaded in the history of the cell's writer once it serves (the spec's ReplicatedSurvives), after every step
- [x] #3 Crashes land anywhere in a take, in a replacement and in a member's append or fence, with the writer's disk kept or lost; retakes on the writer's own disk are among the steps
- [x] #4 MemDisk::crash can also corrupt unsynced blocks (garbage or old contents), and replica logs recover past them as their CRCs say
- [x] #5 With TASK-678 unfixed, the simulation finds it within the seeds CI runs; each finding becomes a seed test
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
AC evidence: profiles replicated (f 1 and 2 on 5 hosts, corrupting crashes, net faults) and retakes, 200 seeds each, green; told-Replicated check after every step (World::check); MemDisk::crash_corrupting + corrupting_crashes_keep_what_was_synced_and_mangle_the_rest + logs_end_at_their_last_whole_frame_after_corrupting_crashes; retakes_need_to_install_their_own_logs finds TASK-678's loss (seed 49) with the fix off; the seeds that found bugs (replicated 33, 166; retakes 7, 129) are within the 0..200 the tests run. Found: the base-gap refusal (trimmed, fixed, then gated on authentication after the review), TASK-683 (filed), passing over unreadable copies (seed 33, fixed), forced_after (seed 129, fixed). Verified on the branch, landed as c9c8234ac..522dcce5b: macOS workspace 2812/2812 and local_cell/bucket_cell/cladefs 1125/1125 (every simulation); ubu1 under fxjob clippy + workspace 3124/3124; TLC on the Mac: every non-deep replication model holds, all 15 mutations violate, deep ReplicationDeferReplaces holds (21.4M states); scripts/upstream-changes.py --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Replicated cells run in local_cell's seeded simulation: replica directories per host, a network that breaks (one way or both), holds, duplicates and drops calls, members driven a turn at a time (Replicator::idle/drive), the keeper on the scheduler (upload::Spawn, ticks not the clock), crashes corrupting unsynced blocks, disks lost with their incarnations; every commit told Replicated checked in the serving writer's history. It found three protocol defects in a night (base gap, TASK-683, unreadable copies passed over) and a misleading forced report. Verified on the branch, landed as c9c8234ac..522dcce5b: macOS workspace 2812/2812 and local_cell/bucket_cell/cladefs 1125/1125 (every simulation); ubu1 under fxjob clippy + workspace 3124/3124; TLC on the Mac: every non-deep replication model holds, all 15 mutations violate, deep ReplicationDeferReplaces holds (21.4M states); scripts/upstream-changes.py --check passes.
<!-- SECTION:FINAL_SUMMARY:END -->
