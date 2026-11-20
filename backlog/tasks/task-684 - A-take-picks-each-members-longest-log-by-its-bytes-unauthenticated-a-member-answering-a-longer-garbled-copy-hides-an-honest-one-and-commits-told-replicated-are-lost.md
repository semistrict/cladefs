---
id: TASK-684
title: >-
  A take picks each member's longest log by its bytes, unauthenticated: a member
  answering a longer, garbled copy hides an honest one, and commits told
  replicated are lost
status: Done
assignee: []
created_date: '2026-11-20 03:05'
updated_date: '2026-11-20 04:25'
labels:
  - durability
  - peers
  - security
dependencies: []
references:
  - crates/local_cell/src/upload.rs
  - crates/local_cell/src/replicate.rs
priority: medium
type: bug
ordinal: 1065000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found 2026-11-19 writing TASK-680's review test. Shared::read_longest keeps, of each log, the copy of the member whose fence answered the greatest end, and replicate::trimmed then reads its records holding no key (wal::replica_records, LogRecord::decode with None). A copy whose frames are garbled past some point reads as a shorter log, its records past it gone, as a torn tail does; one longer than the honest copies but garbled wins the pick and hides them. A member, or anything on the unauthenticated peer network (doc-21: no authentication for now), can so make a take lose commits told replicated, silently, not only deny service as doc-21 says a hostile host can. The take holds the cell's keys by then (TASK-680's review added wal::replica_authentic for the gap case).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 read_longest prefers, of each log, the longest copy whose header and records the cell's keys find authentic, a member's copy that isn't counted as none
- [x] #2 A test: two members, one answering a longer copy with a record garbled or MACed under another key; the take recovers every commit the honest one holds
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
AC evidence: read_longest keeps each segment log's longest authentic copy cut to its whole prefix (wal::replica_whole); an unreadable or short copy, or a log with no authentic copy, fails the take; takes_prefer_whole_authentic_copies_to_longer_garbled_ones passes and fails with the truncation removed (hand mutant); gaps_are_given_up_only_when_authentic covers a copy MACed under another key. Verified on the branch, landed as c9c8234ac..522dcce5b: macOS workspace 2812/2812 and local_cell/bucket_cell/cladefs 1125/1125 (every simulation); ubu1 under fxjob clippy + workspace 3124/3124; TLC on the Mac: every non-deep replication model holds, all 15 mutations violate, deep ReplicationDeferReplaces holds (21.4M states); scripts/upstream-changes.py --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
A take keeps, of each segment's log, the longest copy the cell's keys find authentic, cut to its whole prefix, rather than the copy whose member claimed the greatest end: a longer garbled or forged copy no longer hides an honest one. Verified on the branch, landed as c9c8234ac..522dcce5b: macOS workspace 2812/2812 and local_cell/bucket_cell/cladefs 1125/1125 (every simulation); ubu1 under fxjob clippy + workspace 3124/3124; TLC on the Mac: every non-deep replication model holds, all 15 mutations violate, deep ReplicationDeferReplaces holds (21.4M states); scripts/upstream-changes.py --check passes.
<!-- SECTION:FINAL_SUMMARY:END -->
