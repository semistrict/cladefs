---
id: TASK-537.3.3
title: 'Writer side: flushes streamed to members, the Replicated level and its waiters'
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18T17:02:00Z'
updated_date: '2026-11-18T23:20:00Z'
labels:
  - durability
  - peers
dependencies:
  - TASK-537.3.1
  - TASK-537.3.2
documentation:
  - >-
    backlog/docs/doc-21 -
    Replicated-commits-a-majority-of-hosts-disks-before-the-bucket.md
parent_task_id: TASK-537.3
priority: high
type: feature
ordinal: 933000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-21 step 3. The writer must send each flush records (Held) to its members, keep their ends, and release Replicated waiters on f+1 copies or Uploaded; the lag bound must cover every commit of a replicated cell.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each flush sends its records to every member at once with the local sync; members ends tracked
- [ ] #2 Replicated waiters woken at f+1 copies (the writers own counting) or at Uploaded; none on f = 0
- [ ] #3 fsync level per cell honoured in commit_and_wait; sync family waits for Replicated in a replicated cell
- [ ] #4 Tests: commits released by member acks with the bucket stalled; released by Uploaded with members down
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Replica form: each flush's records as Held (LogRecord::of(.., hold=true), reusing the local ones when they are Held), framed and MACed at their offsets in a replica log whose header is the local log's: a valid segment log, which a taker writes as logs/<cell>/<n> beside an empty raw file and wal::recover re-closes (restore writes Held runs into the raw data). Own-extent files (commits that stored extents on their own, Runs::Synced) as a stream of their own.
2. local_cell::replicate: streams (log n, own extents), a Replicator holding the configuration's member links (trait MemberLink, gRPC in cladefs), each member's synced ends, the replicated epoch (f+1 counting the writer's own Local), and its waiters; drops through a segment once the head acknowledges it.
3. Hook in LocalCell::flush_locked after make_durable: the batch's replica bytes to the Replicator, when the store replicates.
4. Waiters: Replicated released by replicated or uploaded epoch (doc-21's disjunction); lag bound on every commit of a replicated cell; commit levels in OpenCell::commit_and_wait by the head's wish (fsync -> wish.fsync; sync family -> Replicated).
5. cladefs: MemberLink over peers::member (Append stream, keepalive), and a take's first configuration (f >= 1, none in the head): peers picked by rendezvous hashing over the peer list, their incarnations asked, the head swapped to it before the store serves (Writer gains a swap of its head's replication).
6. Tests: the replica log recovers through wal::recover; a Replicator over fake links releases waiters at f+1, not before, and by Uploaded with members down; flush hook; through the daemon with two peer servers.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-18 progress: built local_cell::replicate (Replicator: a thread per member, marks per flush, replicated = f-th highest member epoch, base raised by commits in segments started before it; MemberLink, LinkError; Spool trait, MemSpool, the store's DirSpool in replica-spool/<cell>/ — the user asked for no in-memory replica bytes; restore with trimming against the head's last commit), wal::ReplicaLog (Held records, local header, MACs at replica offsets) and replica_commits, the flush hook (LocalCell::replicate, replicate_to marking segments that already hold records unreplicated), Uploader::wait_replicated/waker/replication, OpenCell commit levels (fsync by the head's wish, sync family Replicated), Writer::configure, upload::Members + the first configuration's install at a take (refused if the head names one already: TASK-537.3.4), cladefs ReplicaMembers (rendezvous pick, Incarnation asked) and GrpcMember (one Append stream per member), CreateCell refusing replicas>=1 without members (ENOTSUP). Tests: replicator over fake links (f=1, f=2, base, lost logs resent, pieces, fenced, acknowledged drops, failing spool), gRPC link to a real member, control refusal; 73/74 passed in the first run (the control test, fixed). Found: the recovery test showed members' logs can begin with segments the bucket holds (drops lag the head): restore now trims against the head's last commit, refusing logs that neither reach it nor go on from it. Pending: rerun of the recovery test, then the two reviews.

Pushed to main as 81a4b0200 (the user: push after compile and unit tests): 86 selected tests pass (the recovery test among them), clippy clean. Not done: the verifying-reviewer and Codex reviews, and the full ubu1 suite.

Reviews done: Codex (aca489437: sends before the local sync, a spool failing breaks replication alone, drops race no reads; waiter wake test) and verifying-reviewer (9f8ae8dbb with 537.3.4's: own extents before records). Blind spots: the waiter test only hit the satisfied path; MemDir never fails a rename over a file in tests. Remaining for Done: the full ubu1 suite (sudo).
<!-- SECTION:NOTES:END -->
