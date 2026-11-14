---
id: TASK-404
title: >-
  extent_store::store commits_reopen_from_their_logs flakes under load: the next
  segment's log not placed once settled ((1, false, 2))
status: Done
assignee:
  - '@claude'
created_date: '2026-11-13 21:06'
updated_date: '2026-11-14 22:24'
labels:
  - testing
  - flaky
dependencies: []
priority: medium
type: bug
ordinal: 788000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen 2026-11-13 in scripts/test.sh's workspace part on t3/395-integrated (the per-cell collector's round-two merge; no extent_store store code changed), the laptop loaded by a TLC run beside it: crates/extent_store/tests/store.rs:466 assert_eq!(logs(&store), (1, true, 2)) got (1, false, 2): after settled() and prepared(), store.placed() is None, the next segment's prepared log not yet placed under its name (TASK-384). Passes 6/6 run alone right after. TASK-370 fixed another counting race in the same assertion (closed logs counted before they went). The test should wait for what it counts (the placement), not the host's speed; or the placement is not covered by settled()/prepared() as the test assumes.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The test waits for the placement explicitly, or prepared() returns only once the prepared log is placed
- [x] #2 Passes 20 runs beside a loaded workspace run
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Find why store.placed() is None after settled() and prepared(): a preparation of the next segment's log whose segment opened meanwhile (stale, or placed but opened before its entries synced) ended there; the opening's prepare_next began none as it ran, nor did the last commit's flush, so nothing was prepared until another flush or closing.
2. Fix the store: such a preparation goes on as the next segment's (Preparing::Again, placed_synced returning the next's), in Store::prepare_next's opener loop.
3. Fail point segments::preparing; store test preparations_overtaken_go_on_as_the_next_segments holding it (red without the fix); segments unit test pins both cases.
4. Before/after: commits_reopen_from_their_logs with preparations slowed by the fail point, stress runs.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-14, coordinator: again on GitHub's macOS workspace job, main 3a01ee0ab (run 43627290622): store.rs commits_reopen_from_their_logs (1, false, 2) != (1, true, 2), failed on both tries. Keeps main's CI red with TASK-385's test_a_migration_restores_on_the_second_run_what_the_first_left (same run).

2026-11-14 (claude): cause in the store, not the test. A preparation of the next segment's log (Store::prepare_next's opener thread) whose segment opened meanwhile (before it placed the log, or after, before placed_synced) ended there; the opening's prepare_next began none as it ran (Segments::to_prepare: preparing), so nothing was prepared until another flush or closing. The test's last commit's flush finding one running left none: prepared() returned with placed() None. Fixed in 00fffc36b: Preparing::Again / placed_synced return the next's preparation, the opener runs on. Fail point segments::preparing; new store test preparations_overtaken_go_on_as_the_next_segments (red without the fix: None vs Some(3)); segments_open_with_their_logs_prepared_syncing_nothing pins both cases. Reproduction: plain stress under 30 busy loops 0/200 (the window needs a slow opener); with preparations slowed by the fail point, commits_reopen_from_their_logs failed with CI's exact (1, false, 2): 41/100 (20 ms), 16/20 (100 ms) before; 0/100 at each after. Unslowed beside a full extent_store suite run: 100/100. extent_store 270/270, test.sh clippy+shuttle+scripts green, upstream-changes --check ok, on macOS; Linux left to main's CI.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
A preparation of the next segment's log overtaken by its segment's opening ended without preparing the next's, and the opening began none while it ran: the next segment had nothing placed until another flush or closing (CI's (1, false, 2)). It now goes on as the next's (Preparing::Again, placed_synced). Deterministic test via fail point segments::preparing; slowed reproduction 41/100 failing before, 0/100 after.
<!-- SECTION:FINAL_SUMMARY:END -->
