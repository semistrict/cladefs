---
id: TASK-537.3
title: >-
  Implement replicated commits: peer append RPCs, replica logs, the Replicated
  level and recovery on take
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18T12:36:00Z'
updated_date: '2026-11-18T17:02:00Z'
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
- [ ] #1 Peer RPCs to append and fetch a cell's log between daemons, with the replica's log on disk bypassing the page cache
- [ ] #2 Per-cell replication options through the control API; replicas picked from the pool in the bucket
- [ ] #3 Commit waiters for Replicated; degraded to Uploaded without a quorum, counted in stats and metrics
- [ ] #4 A take on a new host recovers the replicated suffix; crash and multi-daemon tests show nothing Replicated is lost
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-18, doc-23 (TASK-633): the user chose a bucket per cloud region. doc-21's failure domain (zone or cloud region) is still undecided by the user; doc-23 argues for a zone, as a member outside the cell's home cloud region keeps the tail but gives no availability. Settle it before building the member pool. Members, like peers, are kept per bucket number.
<!-- SECTION:NOTES:END -->
