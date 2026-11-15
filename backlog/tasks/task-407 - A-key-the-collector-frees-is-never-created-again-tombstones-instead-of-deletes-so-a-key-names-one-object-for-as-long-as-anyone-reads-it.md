---
id: TASK-407
title: >-
  Keys written once, commits published by swapping the branch's head: no key is
  ever reused, so nothing reads by versions
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-14 16:01'
updated_date: '2026-11-15 10:08'
labels:
  - bucket
  - collector
  - cache
dependencies: []
priority: high
ordinal: 796000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Segment keys are (branch, seq), and a commit is its segment's create-if-absent: the create is the fence. The collector frees keys, and a taken-over writer's late create can make an object again at a freed key with other bytes (TASK-383). Pins (TASK-388, CreatesNeverFreed) keep the positions where such a create could still be acknowledged, and every reader copes with reuse by versions: footer-over-index (TASK-79.18), Flown's Stored in flight (TASK-92.43), the part cache by Stored, a read again by a tail fetched anew, peers' not-as-asked (TASK-406.2). The user (2026-11-14): a cache key should name something that never changes; leaning on CRCs to find out it did is wrong.

Decided with the user (2026-11-14), over tombstones (the collector leaving a tiny object at each freed key; verified in specs/bucket at c65148c89, notes): every object is written at a key no other write uses, the writer's lease epoch in it, and a commit becomes visible only when the branch's head, one object per branch replacing its lease, is swapped to name it, a conditional PUT (S3 If-Match, GCS generation). The head is the fence: a takeover swaps it to a new epoch, and the old writer's next swap fails. Readers read the head and only what it names. An object of an older epoch that nothing reachable names can never become visible, so the collector deletes it at once, with no pins and no time bound. Compaction's output reaches readers through the writer's next checkpoint, so the head has one writer. A durable commit costs two round trips in series (segment PUT, then head swap) instead of one: the sync loop ~15/s at 25 ms of latency where TASK-371 made it ~30/s. No buckets exist yet, so no migration: a new format.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 specs/bucket models the head: keys written once, commits by the head's swap, takeover and deletion by it; an invariant holds that no key ever names two objects; NoAckedLost and the other invariants hold; mutations of the head's rules each find their violation
- [x] #2 The rules that only guarded key reuse are gone from the spec and the code: fencing by create, fences, gaps, void and late segments, pins (CreatesNeverFreed, collector::pinned), forks_after_lease's lease read
- [x] #3 Writers commit by PUT then head swap; takeover, recovery, deletion and forks go by the head; the collector deletes unreachable objects of older epochs at once
- [x] #4 The fetcher, part cache, in-flight reads and peers key on the object key alone: footer-over-index, Flown's Stored, the read again by a tail fetched anew and peers' not-as-asked are removed
- [x] #5 Durable commit latency measured on ubu1 at 25 ms (sync loop), against TASK-371's 30/s
- [x] #6 doc-7 describes the head and the rule: a key is written once
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Spec first (specs/bucket/Bucket.tla, rewritten in place):
   a. The lease becomes the head: [epoch, token, holder, end, chain, ck]: the committed segments 0..end-1, each by its epoch (chain), and the newest checkpoint <<q, e>>. Segments and checkpoints are keyed <<b, s, e>>; an upload lands always (a key is one writer's).
   b. Commit = the WAL's prefix landed, between commits (acksBetweenCommits), then one CAS of the head (end, chain) by the holder; failing, the writer stops. Takeover = CAS to epoch+1, recovery from the head, no fence segment; the WAL restamped after head.end at the new epoch. Checkpoint = PUT, then CAS of head.ck. Deletion = CAS to Final, recovery from that head.
   c. Readers (forks' recoveries, the collector) read the head, then what it names; finding something gone, they read the head again and start over if it moved (headReread, replacing newestCheckpoint).
   d. Collector: keeps what each live head reaches (and fork points, moves ways as now); deletes any other object of an epoch below the head's; of the head's epoch keeps segments and own extents at or past end, and checkpoints newer than head.ck (one rule, collectorEpochs, replacing pins, orphansAfterSuccessor, extentKeysNameSegments). Heads stay, as records do.
   e. Moves: keyed by the source and the pass's epoch, found by listing the source's moves, so that a fenced pass's late create never reuses a key (today compact_object can recreate moves/<src> after a later pass deleted it).
   f. Rules removed: pins*, tombstones, forksAfterLease, fence, continuity, newestCheckpoint, footers, leasesByTakings, orphansAfterSuccessor, extentKeysNameSegments, deletionsFence, and those of WAL/spans that only fences needed (walAfterFence, walContinues, spansFenced), each checked: its scenario gone, not just its mutation. New mutations: NoHeadSwap, NoEpochKeys, NoCollectorEpochs, NoHeadReread, NoMovesByPass. Invariants: KeysNeverRecreated (a key written once), NoAckedLost and the rest; every model under 10 min (TASK-409).
2. Code: layout (keys with epochs, FORMAT bump), branch/head (the lease object extended), writer (PUT then CAS; ambiguous CAS resolved by reading the head), recovery from the head, deletion, WAL rebase, collector (reach + epoch rule, moves keys), simulation profiles and fault tests.
3. Readers: remove footer-over-index, Flown's Stored, the read again by a fresh tail, peers' not-as-asked; caches keyed by key alone.
4. Measure the sync loop on ubu1 at 25 ms against TASK-371.
5. doc-7, OPEN_QUESTIONS/dead ends: tombstones and pins superseded.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-14, spec (c65148c89, Mac): with tombstones every specs/bucket model holds but BucketStrays and BucketTakeovers (not run: 18 min and 1h47m before; TASK-409): Bucket, Cells, CellsPoints, Compaction, Deletions, Forks (35.0M states, 57 min beside another agent's TLC; doc-7 has 14 min), Inherit, Overlap (13.4M, 36 min, same load; doc-7 14), Own, Placements, Points, PointsDeep, RefsClones, RefsTaken, Spans, SpansCollect, Twins, Wal. All 54 mutations fail as expected; NoTombstones (tombstones off) violates KeysNeverRecreated in 12-43 s: segment 1 of branch 1 freed, then created again by a writer's late create.
Pins under tombstones: each NoPins* mutation with "tombstones" added (NoPinsFences, Takings, LateStrays, OldFences, VoidStrays) holds, no NoAckedLost (3 s to 34 min): a late create meets the tombstone and fails, so tombstones cover what TASK-388's pins guard.
Keys that can be created again once freed (AC2): segments (a taken-over writer's late create; recovery's fence) and checkpoints (a stale writer's checkpoint create) - tombstones cover both, modelled; moves only by their segment's being created again, covered with it. Not: own extents (one key, one content), records, leases, cells, references, marks (changed in place, never read by parts).

2026-11-14, spec done (471b009b1, 02d6a931a and after, Mac): specs/bucket rewritten around the head. All 20 designed models hold (Bucket 131K states 7-11 s, was 1.27M; BucketForks 4.9M 214 s, was 35M; BucketTakeovers 5.5M 575 s, was 207M 1 h 47 min; BucketCells resized to one segment, its fence's gone: 1.58M 78 s); all 36 mutations find their violations, new: NoHeadSwap (NoAckedLost), NoEpochKeys and NoMovesByPass (KeysWrittenOnce), NoCollectorEpochs (NothingNeededGone). Removed with the user's leave: 22 mutations of rules gone (pins x5, tombstones, forksAfterLease, fence, continuity, newestCheckpoint x2, footers, leasesByTakings, orphansAfterSuccessor, extentKeysNameSegments, deletionsFence, deletionsReread, walAfterFence, spansFenced, walBetweenCommits, walContinues, spansHeld). Re-modelled: NoCandidatesFirst and NoForkClaimsLanded need a second commit (their old scenarios went through a lease deleted, a fence); NoMovesTakenFirst needs the swap's answer before the moves are taken (a step of its own); NoWaysFromSeals's scenario is a fork's now (a stale writer's checkpoint is no head's, but forks list their parents' checkpoints).

All ACs verified (subtasks 407.1-.5 done, TASK-424/429/430 done): spec 56 models and mutations passing; code pushed through 664e48aca; macOS gate 2488/2488, sim 50/50, go, scripts; ubu1 workspace 2786/2787 (TASK-315 only), Shuttle passing; durable commit ~60 ms vs ~33 ms at 25 ms (one round trip more). Left: TASK-407.6 (fork-point checkpoint re-creatable after retirement: garbage, not a wrong read).
<!-- SECTION:NOTES:END -->
