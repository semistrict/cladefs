---
id: TASK-537.1
title: 'Research: the replication and recovery protocol for replicated commits'
status: Done
assignee:
  - '@claude'
created_date: '2026-11-18T12:36:00Z'
updated_date: '2026-11-18T12:36:00Z'
labels:
  - durability
  - peers
dependencies: []
parent_task_id: TASK-537
priority: high
type: spike
ordinal: 928000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Before any spec or code, the protocol has to be chosen against prior art and against what doc-7's bucket fencing already gives. Candidates: PacificA/BookKeeper-style ensembles fenced by an external authority (here the bucket), Raft log replication without its elections, Kafka's ISR, Aurora's quorum log with segment fencing, and Delos/Virtual Consensus loglets sealed by a metastore. Points to settle: how an epoch change seals the old replica set; which suffix recovery takes (longest acknowledged by a majority versus what was told durable); how a replica's log is truncated once the bucket acknowledged a segment; how replica replacement re-replicates the tail; and how this sits with the existing local WAL format (open segment raw file and log).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A design doc in backlog/docs compares the candidate protocols and records the chosen one, with the reasons
- [x] #2 The doc defines the messages, the replica's on-disk state, the bucket's new fields (replica set, epoch), the commit and recovery steps, and the safety and liveness properties the TLA+ spec must check
- [x] #3 dead-ends/ was checked; decisions are recorded in OPEN_QUESTIONS.md
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Map the commit, flush, upload, take and recovery code and the peer network (done: file:line in doc-21).
2. Compare prior art (Raft, PacificA, ISR, BookKeeper, Delos, Vertical Paxos, Aurora, Neon, Durable Objects).
3. Choose the protocol and write doc-21: messages, member state, head fields, commit, take, replacement, degraded mode, properties and mutations for the spec, the failure rows, costs, steps.
4. Record decisions in OPEN_QUESTIONS.md.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
dead-ends/ has nothing on replication, peers' logs or quorums (rg replica|quorum|peer log). doc-21 written. Notable findings: log records are self-contained when Held (XTS ciphertext + MACed Contents), so a member mirrors logs and recovery reuses wal::recover; Wal::rebase must accept a tail starting after last_commit; the peer channel's 5 s request timeout needs a separate streaming endpoint; no waiter exists for Local today, so Replicated needs its own Notify; the lag bound applies only to Ordered today and must apply to every commit of a replicated cell.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
doc-21 picks the replication protocol: primary-backup where each configuration of the writer plus 2f peers is acknowledged by a quorum of f+1 of its 2f+1 copies, and only the bucket's head CAS changes configurations. A take seals f+1 non-writer members, recovers from the greatest end, installs c+1 and activates it before serving; peer replacement uses a joint rule while its swap is in flight; Replicated waiters are also released by Uploaded. Members mirror the writer's segment logs with Held records, so recovery reuses wal::recover and Wal::rebase. The doc lists the RPCs, head fields, properties and mutations for TASK-537.2, rows for doc-12, costs and steps. The user reviewed it 2026-11-18. Deviation from the interview: the pool is the existing peer list (OPEN_QUESTIONS.md).
<!-- SECTION:FINAL_SUMMARY:END -->
