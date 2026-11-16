---
id: TASK-449
title: >-
  A restore makes 13 bucket round trips one after another (21 on a cold host):
  0.41 s at 30 ms a request, 1.3 s at 100 ms
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 12:58'
updated_date: '2026-11-16 07:09'
labels:
  - perf
  - review-2026-11-15
dependencies: []
references:
  - TASK-443.5
  - TASK-443.14
documentation:
  - docs/measurements/macos-bucket-lifecycle-2026-11-15/note.md
priority: high
type: enhancement
ordinal: 849000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Measured by TASK-443.5 (cladefs remote::lifecycle::bucket_lifecycle_by_phase, macOS, a bucket in memory 30 ms a request away; docs/measurements/macos-bucket-lifecycle-2026-11-15). A fork of a point and the fork's first open, the point open on the host: 14 requests in 13 round trips in series, never more than 2 in flight: 414 ms of bucket time at 30 ms a request, 1,325 ms at 100 ms, the same at 1,024, 32,768 and 262,144 extents. The fork: 6 requests, each after the last (child's record GET, create, the reference's create, the point's record GET, the child's record GET again, its update). The open: 8 requests in 7 round trips (child's record and head at once; head again; the base's head; the cell GET; the cell create; the head create; the record again). The child's record is read 4 times and written twice, its head read twice then created, for a branch this host made 0.2 s before. On a host that never saw the point: 25 requests in 21 round trips, 672 ms at 30 ms, 2,138 ms at 100 ms, and the base's checkpoint downloaded (108 KB at 32,768 extents, 856 KB at 262,144); the point's store is opened twice over (ExtentStore::take_with: create_placed opens and drops it, then host_store::take opens it again: 11 requests where the first open's 6 would do). Every restore pays this before its VM boots; nothing in it grows with the base, so it is felt at any size, in proportion to the bucket's distance. Not measured here: the fsyncs of the same path on a shared disk (TASK-443.5, Linux).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A restore on a warm host costs at most half the round trips in series it does today (13), shown by bucket_lifecycle_by_phase and pinned by a test counting its requests
- [x] #2 A point taken from the bucket is opened once, not twice (cold point-open)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Baseline (this tree, 2026-11-16, 30 ms, 1,024 extents): warm restore 14 requests / 13 in series; cold 25 / 21 (busy-time 22.4).
1. Measure: Asked gains the longest chain of requests each begun after the last ended (round trips in series, exact, not timing-derived); a non-ignored test pins a restore's requests and that chain, warm and cold.
2. Fork of a point (6 -> 4 in series): no child_free GET for a point (fork_point's create finds any record there: IdTaken/Deleted as before); finish_point_fork updates the record from the version its create left (transition gives the object it wrote) instead of reading it back; the child's cell held (made, this placement, its volume) at once with the reference, after the record, when the host has no KMS and the fork inherits no keys.
3. What the host knows, extent_remote::known on the host bucket (Bucket::known): (a) segment epochs heads named, immutable once named (runs only grow): state_at reads a fork point's checkpoint key without the origin's head; (b) what a host learned of a branch it is about to open (its record as its fork or take left it, its cell as held, headless): taken by the store's first open, skipping the record, head and cell reads; every write after stays conditional and the record is read again after the take, so a hint is a read from earlier (the model's staleTake/OpenGone).
4. Open: the head read with the record is what recovery starts from (no second head GET); a point reads no head ever (PointsUnwritten); the cell read at once with record and head when nothing is known.
5. Cold host: ExtentStore::take_with opens the store create_placed made instead of dropping and reopening it; the record create_placed's check read is learned.
6. block_on's throwaway runtimes: count S3 connections per restore against s3_server (connections counted); a shared runtime if they die with it.
7. specs/bucket: action property NamedEpochsStay (a segment a head names keeps its epoch) in models with takeovers/checkpoints/deletions/removals; all models and mutations pass. doc-7 updated.
8. bucket_lifecycle_by_phase before/after at 1,024 and 32,768, 30 and 100 ms; nextest extent_remote/extent_store/cladefs + sim; clippy.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Built and measured on macOS, 2026-11-16 (docs/measurements/macos-restore-round-trips-2026-11-16; bucket_lifecycle_by_phase gained a chain column: the longest chain of requests each begun once the one before was answered).
| restore | requests | round trips in series | bucket ms at 30 / 100 ms |
| warm, before | 14 | 13 | 418 / 1,327 |
| warm, after | 7 | 6 | 192 / 614 |
| cold, before | 25 | 21 | 673 / 2,145 |
| cold, after | 11 | 9 | 289 / 921 |
Same at 1,024 and 32,768 extents; bytes up 286 both; down 138 warm (389), cold one checkpoint as before. By phase after: cold point-open 4 in 3 (11 in 8; opened once); fork 5 in 4 (6 in 6); fork-open 2 in 2 (8 in 7). Also reopen 6 in 4 (7 in 6), short life 22 in 21 (29 in 28).
What changed and why each is safe (doc-7, What a host knows): (1) segment epochs a head named kept for good (extent_remote::known; runs only grow) - spec action property NamedEpochsStay added to Bucket, BucketTakeovers, BucketDeletions, BucketRemovals, BucketForks, BucketPoints, BucketFences, BucketCompaction, BucketResumes; probed: a takeover changing an epoch violates it at once. (2) What a host learned of a branch it opens next (record its fork/take left, cell its fork held, headless): equals a read made earlier; all later steps conditional or re-read (the record after the take): the model's staleTake/OpenGone (BucketForks, BucketRemovals). (3) no child_free GET for a point fork: the record's create refuses any record there; another's (deleted or not) now IdTaken, ours deleted Deleted (was Deleted for another's deleted record, which mapped to ENOENT 'point deleted'). (4) the clone's update from its create's version (transition_leaving): conditional as from a read. (5) the clone's cell held by the fork at once with the reference, after the record (no KMS, clear only; TASK-485 filed for sealed). (6) a point reads no head (PointsUnwritten). (7) open reads record, head, cell at once; recovery from that head. (8) take_with opens a taken store once.
Connections: upload::block_on built a current-thread runtime per call; a bucket's connections are tasks of the runtime that opened them, so they died with it. Measured against s3_server (now counts connections; exposed under extent_remote's testing feature): with per-call runtimes each restore's fork opened 1 new connection (fork, open) = [(1,0),(1,0),(1,0)] over 3 restores, the host with its own client; one process runtime (2 workers): [(0,0)x3]. extent_remote requests_on_runtimes_dropped_after_connect_anew: per-call runtimes [1,1,1], shared [1,0,0]. Fixed: block_on on one runtime for the process.
Tests: new restores_ask_the_bucket_what_the_protocol_needs (pins per-phase (requests, chain); red with child_free back: forks (6,5)), restores_connect_to_the_bucket_no_more (red with per-call runtimes: [(1,0)x3]), requests_on_runtimes_dropped_after_connect_anew, knowledge_is_bounded_and_learnings_taken_once. Updated: takes_recover_their_branches_once_by_their_segments_tails (get 5 -> 4, one head read), clones_refused_by_records_there_* (another's deleted record: IdTaken), RESTORE_STEPS without remote::child_free.
Results: nextest workspace part filtered to extent_remote|extent_store|cladefs: 943 run, 943 passed (74 skipped); sim part 53/53; clippy as test.sh (three invocations) clean; cargo fmt --check clean; scripts/upstream-changes.py --check passes. Decisions in OPEN_QUESTIONS.md. Linux (ubu1) not run: no access in this task.

TLA+ on this Mac: the nine models carrying NamedEpochsStay all PASS (Bucket 7s, BucketTakeovers 525s, BucketDeletions 117s, BucketRemovals 44s, BucketForks 136s, BucketPoints 34s, BucketFences 4s, BucketCompaction 77s, BucketResumes 51s); all 41 bucket mutations violate their invariants as expected (unchanged by this change: actions untouched). Commit e97fb04b5 on branch t449 (pushed; not main).
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
A restore now asks the bucket 7 requests in 6 round trips one after another (14 in 13 before), 11 in 9 on a host that never saw the point (25 in 21), its point opened once: 192 ms of bucket time at 30 ms a request (418), 614 ms at 100 ms (1,327). The host reads no more what it knows (extent_remote::known: segment epochs heads named, for good; what it learned of a branch it opens next), the fork skips the ID look, updates from its create's version and holds the clone's cell beside the reference, the open reads record, head and cell at once, and calls from no runtime's task share one runtime so their connections live on. Verified by bucket_lifecycle_by_phase (1,024 and 32,768 extents, 30 and 100 ms), restores_ask_the_bucket_what_the_protocol_needs and restores_connect_to_the_bucket_no_more (each red under its hand mutant), TLC (nine models with NamedEpochsStay, 41 mutations), nextest 943/943 and sim 53/53, clippy and fmt clean. Linux run open (no ubu1 in this task); sealed restores: TASK-485.
<!-- SECTION:FINAL_SUMMARY:END -->
