---
id: TASK-537.3
title: >-
  Implement replicated commits: peer append RPCs, replica logs, the Replicated
  level and recovery on take
status: Done
assignee:
  - '@claude'
created_date: '2026-11-18T12:36:00Z'
updated_date: '2026-11-19T13:56:00Z'
labels:
  - durability
  - peers
dependencies:
  - TASK-537.2
parent_task_id: TASK-537
priority: high
type: feature
ordinal: 930000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The implementation follows the spec and is built on the peer network of TASK-406.2.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Peer RPCs to append and fetch a cell's log between daemons, with the replica's log on disk bypassing the page cache
- [x] #2 Per-cell replication options through the control API; replicas picked from the pool in the bucket
- [x] #3 Commit waiters for Replicated; degraded to Uploaded without a quorum, counted in stats and metrics
- [x] #4 A take on a new host recovers the replicated suffix; crash and multi-daemon tests show nothing Replicated is lost
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-18, doc-23 (TASK-633): the user chose a bucket per cloud region. doc-21's failure domain (zone or cloud region) is still undecided by the user; doc-23 argues for a zone, as a member outside the cell's home cloud region keeps the tail but gives no availability. Settle it before building the member pool. Members, like peers, are kept per bucket number.

2026-11-19, the user: no failure-domain rule. A commit may be replicated to any other host in the region (the hosts sharing the cell's bucket); peers carry no domain label. doc-21 and doc-23 updated.

2026-11-19: all six subtasks Done. Full ubu1 suite at 54adcd5ca passes (3037 workspace tests, cladefs incl. cladefs::replicated, every guest case; the pgbench pair on a rerun after a scratch ENOSPC). DoD 1: no macOS run. DoD 2: upstream-changes --check fails on crates outside this work (zx*, storage_units, test_util, storage_xts).
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Replicated commits are built: peer append and fetch RPCs with O_DIRECT replica logs (537.3.2), per-cell f and fsync level through the control API with members picked from the region's peers (537.3.1, .3), Replicated waiters at f+1 copies or Uploaded (537.3.3), take-time fencing and recovery of the replicated tail (537.3.4), member replacement after 30 s, degraded mode and the f = 0 drop, with stats and metrics (537.3.5), and end-to-end tests with real daemons (537.3.6). Verified by the full ubu1 suite and the TLA+ models and mutations. Open: one doc-12 row (a replicated cell's daemon killed and reopened on the same disk) has no test; no macOS run.
<!-- SECTION:FINAL_SUMMARY:END -->
