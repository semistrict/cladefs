---
id: TASK-455
title: >-
  Dead cells cost every sweep 5 requests and 5 objects for ever, and a base's
  pass lists a mark for every clone ever retired
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 13:16'
updated_date: '2026-11-16 04:39'
labels:
  - perf
  - review-2026-11-15
dependencies: []
documentation:
  - docs/measurements/macos-gc-dead-cells-2026-11-15/note.md
  - docs/reviews/2026-11-15-sandbox-performance.md
priority: high
type: bug
ordinal: 849000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Measured by TASK-443.4 (docs/measurements/macos-gc-dead-cells-2026-11-15, file buckets on macOS, 100 to 10,000 cells, exact at every size). (a) A cell whose branches are all deleted keeps 4 objects for good (cell object, record, head, gc/lease: 422 bytes), plus a retired/ mark under its parent's cell if it was a clone, and every later sweep spends 5 requests on it: 2 GETs and 2 conditional PUTs of its lease and 1 LIST of its prefix, skipping it. A sweep's requests are 5 x every cell ever made. Extrapolated, not measured: at 10^5 sandboxes a day, after 30 days a sweep is 1.5x10^7 requests (6x10^6 GET, 6x10^6 PUT, 3x10^6 LIST), some hours at 16 passes at once on a bucket 30 ms away, and about USD 47 a sweep at S3's list prices as remembered (unverified), growing by a day's sandboxes every day; garbage waits as long as a sweep takes. At 10^3 a day it is 1.5x10^5 requests after 30 days: not felt. (b) The pass over a cell lists its whole prefix before deciding to skip (passed::list), retired marks included: a base whose point was cloned N times lists N marks every pass, N/1,000 LIST pages (counted as 1 request), about 1 KiB of memory a mark (19 MiB at 10,000; about 1 GiB at 10^6, extrapolated), and it skips only if no clone was made or retired since its last pass, so a base in use goes in full every sweep. Also seen: a dead cell is passed in full twice before it skips, the second pass creating its marks again. The rules that keep these objects (records kept, TASK-174; heads final, TASK-407; marks kept for good, TASK-395.13/.14; the lease) each have a safety reason, in OPEN_QUESTIONS.md's decisions on the per-cell collector and specs/bucket: a fix has to keep them or replace them in the spec.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A sweep's requests do not grow with the cells whose branches were all deleted before its previous sweep: scripts/gc-sweep.sh with CLADEFS_GC_FILL=--deleted at 100, 1,000 and 10,000 cells shows the third sweep's requests the same, or bounded by a stated constant, at each
- [x] #2 The objects a deleted sandbox leaves in the bucket for good, its mark under its parent's cell among them, are none or bounded by a stated constant however many sandboxes were made; the same runs' objects left show it
- [x] #3 A pass over a base's cell lists and holds a number of objects that does not grow with the clones made and deleted before its last pass: CLADEFS_GC_FILL='--clones 1024' CLADEFS_GC_CELL=1/1 at the three sizes shows its objects listed, LIST pages and peak memory flat
- [x] #4 A LIST of many pages is counted as its pages in cladefs-gc's request counts
- [x] #5 specs/bucket checks whatever replaces the rules that keep records, heads, leases and marks, with a mutation each; cladefs-gc-fill's test dead_cells_keep_four_objects_and_cost_a_sweep_five_requests is changed to the new numbers
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Design: a dead cell is removed whole by the pass that finds it dead, once every record in it is one no recorder makes again: a root's or a made clone's (a new record kind, Clone, what Forking becomes), of an ID an allocator gave (its block's object there; block 0, the tests' named IDs, never given). Such records are made once, by their makers alone: forks never name an allocator's ID (doc-7 Object IDs), so no parent's writer records them, and a clone's restore wrote its reference before it was made. So: (a) its references are retired unmarked (no stale writer exists), so a base's cell keeps no mark of a clone made and gone; (b) the dead cell's objects all go, its records last, then its lease, after the retirements. Writable forks' children, points, claims, deletions' own records and clones never made keep their records and marks as now (recorders may make them again). Stale actors after a removal (a deletion run again, a writer's late segment, a stale taker's head) leave garbage the next pass deletes, but a deletion's record of none.
1. specs/bucket first: Given (the allocator's IDs), forks and contests naming none of them (rule idsGivenOnce), restores taking a Given ID once; GcFinish removes a dead cell of given roots and made clones (rule recordsKept now says which records may go: without it, any dead cell's); a made given clone's retirement unmarked (rule clonesMadeUnmarked: without it, every clone's, made or not); a stale take after a removal leaves a head, which a pass over a cell with no record deletes. New invariant RemovedStayGone (a removed branch never lives again). Mutations NoIdsGivenOnce, NoClonesMadeUnmarked, NoRecordsKept rewritten; every model and mutation run with scripts/check-tla.sh, each under 10 minutes, split if not.
2. extent_remote: record::Kind::Clone (format 12); objectid: block 0 never taken, objectid::given; collector: Pass::removal (dead, records all removable, no marks of its own), retirements unmarked for made given clones, removal deletes after retiring (records last, lease last), a cell of nothing but its lease loses it, heads of record-less cells go; a pass over a dead cell records its listing less what it deleted so it is not passed in full twice; a pass first looks (GET lease, LIST) and writes nothing when the record holds and nothing is due, taking the lease only to act; LIST counted by pages in sweep::Counted and Store; Rules ids_given, clones_made_unmarked; Report::removed; cladefs-gc output and metrics.
3. cladefs-gc-fill: allocator IDs for roots and sandboxes; its test renamed and changed to the new numbers.
4. Tests (red without the change): sweeps over N dead cells leave none of their objects and cost later sweeps only the delimiter listing; a base's pass after N clones made and deleted lists a bounded number of objects; rule tests for ids_given and records_kept and clones_made_unmarked; sim test_needs for ids_given (usurped); existing collector, sim seeds, crash and fork-fault tests.
5. Measurement redone with note.md's commands into docs/measurements/macos-gc-dead-cells-2026-11-16 (dated today: 2026-11-15-after); before/after table in notes.
6. doc-7, OPEN_QUESTIONS.md decisions; nextest of extent_remote, extent_store, cladefs as test.sh runs them, the sim part, clippy; commit, push to t455.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Spec first (specs/bucket): Given, gone, OpenGone, GcFinish removal, Unmarked retirement, RemovedStayGone; mutations NoIdsGivenOnce, NoClonesMadeUnmarked caught; NoRecordsKept rewritten, caught; BucketRemovals probed (clone removed, unmarked retirement and a stale head reached). Code: record::Kind::Clone (format 12), objectid::given (block 0 reserved), Record::removable/retires_unmarked, Pass::removal (records last, lease last), heads of record-less cells go, Collector::look, recorded_listing, LIST pages counted, rules ids_given and clones_made_unmarked, Report::removed, cladefs-gc line and metric. Bug of my own the sim found (9 profiles): the post-delete digest recorded with moves of deleted objects left, which then stayed for good; fixed (only when what is left is what a dead cell keeps) with test dead_cells_kept_lose_the_moves_of_what_their_first_pass_deleted, red with the bug back. Blind spot: no unit test had a dead cell holding moves; the sim's uncollected oracle was what checked it. Structure: the collector's rule for what a dead cell keeps now lives in one place per decision (removal, recorded_listing share it); fine.

Before/after, third sweeps (docs/measurements/macos-gc-dead-cells-removed-2026-11-15, same commands as macos-gc-dead-cells-2026-11-15):
| bucket | cells | requests before | after | objects left before | after (objectid/ blocks) |
| deleted | 100 | 502 | 1 | 400 | 7 (7) |
| deleted | 1,000 | 5,002 | 1 | 4,000 | 12 (12) |
| deleted | 10,000 | 50,011 | 1 | 40,000 | 5 (5) |
| clones | 102 | 512 | 6 | 525 | 26 (3) |
| clones | 1,002 | 5,013 | 6 | 5,025 | 28 (5) |
| clones | 10,002 | 50,022 | 6 | 50,025 | 26 (3) |
Requests a dead cell a sweep: 0 after (5 before). Objects a deleted sandbox leaves: 0 (5 before; the allocator's ID blocks, one per filling thread, are no sandbox's). Base cell 1/1 after N clones (100/1,000/10,000): objects listed 22/22/22 (before 122/1,022/10,022), LIST pages 1/1/1 (1/2/11), unchanged pass 2 requests and 9 MiB at every N (5 and 10-19 MiB), full pass 31-32 MiB (31-39). Third sweeps 0.0 s at every size (110.2 s and 188.9 s at 10,000 before).
AC #5: the rules replacing records kept: recordsKept (which records, and with them heads, cell object and lease, go: only a dead cell's of given roots and made clones; mutation NoRecordsKept), idsGivenOnce (mutation NoIdsGivenOnce), clonesMadeUnmarked (marks; mutation NoClonesMadeUnmarked); invariant RemovedStayGone. The cell object and the collector lease are not in the model (as before: passes are fenced by their global numbers); their removal's safety is argued in doc-7 (Dead cells removed whole) and tested (collector, sweep, fill, sim).
TLA+ on this Mac (loaded by other agents, load 27-38), every model and mutation PASS: Bucket 10s, BucketCells 100s, BucketCellsPoints 50s, BucketCompaction 90s, BucketDeletions 101s, BucketForks 149s, BucketInherit 47s, BucketOverlap 314s, BucketOwn 2s, BucketPlacements 5s, BucketPoints 74s, BucketPointsDeep 79s, BucketRefsClones 8s, BucketRefsTaken 12s, BucketRemovals (new) 45s, BucketSpans 14s, BucketSpansCollect 20s, BucketStrays 334s, BucketTakeovers 303s alone (668s under cargo tests), BucketTwins 1s, BucketWal 1s; mutations each violating its invariant: NoIdsGivenOnce 43s, NoClonesMadeUnmarked 2s, NoRecordsKept 93s, NoReferencesRelisted 372s (Given none: 1407s+ with the root given, so set to the model's size before), NoWaysFromSeals 371s, NoPassesFencedStop 203s, NoReferencesHeld 211s, NoRecordsRelisted 194s, NoRecoveriesByOrigins 168s, NoClonesReferFirst 120s, the rest under 10s; dax-cow, follower, journal models and mutations and follower/traces pass. First full run ended on a full disk (shared volume; others' targets); the rest rerun after freeing my scratch.
Tests: cargo nextest -p extent_remote 321 passed; workspace part as test.sh runs it 2518 run, 2514 passed, 4 failed (extent_remote's tests/gc.rs, which ran a format-11 cladefs-gc another worktree built into the shared target mid-run), rerun alone 8/8 passed; sim part 52/52 (with new collection_needs_ids_given, restores_need_clones_made_unmarked). Clippy as test.sh runs it: clean. Red without the change (hand mutant, removal and unmarked retirement off): dead_cells_go_whole_and_cost_later_sweeps_a_listing_however_many, bases_keep_nothing_of_clones_made_and_gone, deleted_roots_keeping_their_records_need_ids_given, the fill test; dead_cells_kept_lose_the_moves_of_what_their_first_pass_deleted red with its bug back; listings_are_counted_by_their_pages red without page counting. scripts/upstream-changes.py --check and test-weakening.py pass.
Not done: DoD #1 on Linux (no ubu1 in this task).

Review fixes (independent review, both findings verified against the code first): (1) a deletion's final head of a branch of no record was deleted by a pass (fates' goes(records.is_empty()), and removal took heads of record-less branches), so a fork's child deleted before its parent's slow writer recorded it could be taken again and marked deleted under its writer; (2) a clone finished by another host (upload.rs record() -> finish_point_fork) had its reference retired unmarked, the host that began it writing it late, for good. Looking around found a third: that store path made the clone with NO reference, so a host dying between record and reference left a clone that lost its data once its point went (the sim finds it: restores_need_resumes_refer). Model first: specs/bucket gains deleteEarly (a fork's child deleted before its record), resume (a second host resuming a clone being made), Record.resumed, invariant DeletedUntaken; TLC on the old rules: NoFencesKept 22 states (DeletedUntaken), NoResumesMarked 26 (ReferencesRetiredMarked), NoResumesRefer 14 (ReferencesStand). Rules: fences_kept (a head of no record listed goes only if not final and a GET finds no record; a final one stays and keeps its cell), resumes_refer and resumes_marked (branch::resume_point_fork says the record resumed with a condition, then refers, then makes; fork_point finding its own record being made does the same; retires_unmarked excludes resumed clones). Format 13 (Record.resumed). Models BucketFences, BucketResumes (1 pass; 636 s with 2). Tests, red under the old rules (the _need_ variants): deletions_fences_stand_until_their_records / _need_fences_kept, late_references_of_resumed_clones_go / _need_resumes_marked, clones_resumed_keep_their_references / _need_resumes_refer; uncollected keeps fences. Blind spots: the model had no deletion of an unrecorded child and only the restore's own host finished a clone; GcFinish read head finality nowhere. The simulation generates neither interleaving for (1)/(2): TASK-149.48. Structure: the head's fate now has one rule (fence) shared by fates and removal; fine. Results: TLC all 64 bucket models/mutations PASS (max NoReferencesRelisted 272 s, BucketOverlap 259 s, BucketTakeovers 226 s, BucketResumes 48 s, BucketFences 4 s); nextest extent_remote+extent_store+cladefs (workspace part) 933/933; sim part 53/53; clippy as test.sh clean; measurement at 100 redone: requests identical every sweep, base pass unchanged, only the allocator's ID-block count differs.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Dead cells of given roots and made clones are removed whole by their first pass in full (records last, lease last), clones' references retire unmarked, idle cells cost a look (lease GET + LIST, no writes), a dead cell kept is not passed in full twice, LISTs count pages. Spec first (rules recordsKept rewritten, idsGivenOnce, clonesMadeUnmarked; invariant RemovedStayGone; model BucketRemovals). Third sweeps: 1 request at 100-10,000 deleted roots (502-50,011 before), 6 over sandboxes (512-50,022); a base's pass lists 22 objects whatever the clones (122-10,022). Verified by TLC (all models/mutations), extent_remote/workspace/sim nextest, the measurement. Linux run open (no ubu1).
<!-- SECTION:FINAL_SUMMARY:END -->
