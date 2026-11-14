---
id: TASK-328
title: >-
  fxdax's images_dropped_as_their_pass_runs_leave_their_bucket_whole fails under
  load: its last collector pass deletes nothing (Report { deleted: 0, compacted:
  0 })
status: Done
assignee:
  - '@claude'
created_date: '2026-11-10 13:14'
updated_date: '2026-11-14 23:49'
labels:
  - testing
  - flaky
dependencies: []
priority: high
type: bug
ordinal: 727000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen 2026-11-10 on ubu1 (TASK-320's branch at c8e4ff56, scripts/test.sh workspace part, target dir on the ext4 scratch disk, other agents' jobs running): storage::kept::tests::images_dropped_as_their_pass_runs_leave_their_bucket_whole (crates/fxdax/src/storage/kept.rs, TASK-291.3) took 80.0 s and panicked at kept.rs:475, `assert!(report.deleted > 0)`: Report { deleted: 0, compacted: 0 }. Run alone right after, on the same build: passed in 13.4 s. Nothing in the branch touches the collector or kept.rs (it moves fxdax's examples and adds release-build guards). Likely a timing assumption: the pass is held up by fail::cfg("collect::pass", "sleep(500)"), and under load the images' closes, or what the pass may delete by then (quicken(BUSY)), land differently, so the pass after finds nothing to delete. The test should say what it waits for, not depend on the host's speed.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The test passes under load (e.g. 20 runs beside a loaded workspace run on ubu1), or fails only on a real loss of the bucket's wholeness
- [x] #2 Its wait for what the last pass deletes is explicit, not a timing
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (GCE, scripts/test-gce.sh)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Reproduce on the Mac (stress the single test), instrument passes to see which pass deletes what.
2. Root cause: the reopening's commits start BUSY passes (grace 0, as often as one ends) after the failpoint is torn down; whichever of them sees the clone's shutdown checkpoint deletes the last garbage, leaving pass_now nothing. The test assumed pass_now is the first pass to see it; the code promises no such order.
3. Test: keep passes held up through the reopening (the reopened images dropped as their passes run too: makes the bad order certain), hold the bucket's Collecting across it so its passes are counted, assert passes after the drops (theirs + one alone) delete > 0, and collected_alone leaves nothing (TASK-390).
4. Prove: old assertion with passes held through reopening red every run; fixed test 0 failures over many runs under load; A/B before/after TASK-400/401.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-13 (TASK-395.6/.8 batch, branch t3/395-incremental-usage): fails 3 of 6 runs alone on the macOS laptop (cargo nextest run -p fxdax images_dropped_as_their_pass_runs), also with the new skipping of unchanged cells turned off by hand, so not that change; once in scripts/test.sh's workspace part, passing in two runs before.

2026-11-13 (TASK-395.15/.16 batch, file bucket locks per directory, fxdax-gc passes spawned): failed once in cargo nextest -p extent_remote -p fxdax, and 1 of 3 runs alone on the macOS laptop; same symptom (Report all 0), as before the batch.

2026-11-14 (TASK-395.13/.14 batch, macOS laptop at load ~48): failed in scripts/test.sh's workspace part and once more alone (Report all 0); then 4 of 4 alone passed. Same symptom.

2026-11-14 (claude, TASK-400's agent): seen again on ubu1 at dddfea15e, in the Linux workspace suite under fxjob (--test-threads 4) and 1 of 3 runs alone after: Report { deleted: 0, compacted: 0, ... }.

2026-11-14, coordinator: now failing on main's CI on both test (workspace) and macos (workspace) in one run: e9b6ac5dc (run 43648106574) and 7036a9bde (run 43641464580), Report { deleted: 0, ... } at kept.rs; passed on 355147cd3 between. Since TASK-400/401 (85b163d62, 9c93d8d08) landed it is the one test keeping main red; check whether they made it likelier.

2026-11-14 (claude): root cause, a test race. After the drops the test tore the failpoint down and opened both images again; those mounts' and closes' commits start BUSY passes (grace 0, as often as one ends) on the bucket's collector. Whichever of them saw the clone's shutdown checkpoint (written as its uploader stops, before Remote's drop returns) deleted the last garbage, the reopening's own freed metadata and the held-up pass's compaction sources, so pass_now found nothing. Instrumented traces: in every failing run the last background pass deleted 2 objects and pass_now 0; in passing runs that pass ran before the checkpoint (skipped: 1) and pass_now deleted the 2. No product bug: the code promises no order between those passes and the test's pass alone.
Not TASK-400/401: A/B on the Mac, concurrently, 20 runs each: 1e3253239 (before 85b163d62) 12/20 failed, 9c93d8d08 (after 9c93d8d08) 7/20. Not TASK-395.18/19 either: 3 of 6 failed on 2026-11-13, before 590941889. On main before the fix, 3/12 and 5/8 failed alone on the Mac. CI going red on both platforms in one run is that rate, not a new cause.
Fix (17898693d): the passes stay held up through the reopening (the reopened images dropped as their passes run too: makes the failing order certain); the test holds the bucket's Collecting across it so those passes are counted (collected() waits for the running one); assert the passes after the drops, theirs plus one pass alone, deleted > 0; then collected_alone leaves nothing (TASK-390, as model_tests' end). Red without the fix: the old assertion under the held-up reopening failed 6/6. Green: 30/30 at load avg 43-50, then 20/20 beside the fxdax suite; fxdax suite 348/348 on macOS; clippy adds nothing in kept.rs; upstream-changes.py --check passes.

2026-11-14, coordinator: main's CI green on efa48fa88 (run 43653116878): test (workspace) on Linux and macos (workspace) both pass, with this fix in.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Test race, not a product bug: after the drops, the reopened images' commits start passes of the bucket's collector (grace 0, as often as one ends), and whichever saw the clone's shutdown checkpoint deleted the last garbage before the test's pass alone, which then reported all 0. The test now holds the passes up through the reopening too (making that order certain), holds the bucket's collector across it so their deletions count, asserts the passes after the drops deleted something and that passes alone then leave nothing uncollected. Old assertion 6/6 red under the held reopening; fixed test 50/50 under load on the Mac (was 3/12 to 12/20 failing); fxdax suite 348/348. TASK-400/401 did not make it likelier (A/B 12/20 before vs 7/20 after).
<!-- SECTION:FINAL_SUMMARY:END -->
