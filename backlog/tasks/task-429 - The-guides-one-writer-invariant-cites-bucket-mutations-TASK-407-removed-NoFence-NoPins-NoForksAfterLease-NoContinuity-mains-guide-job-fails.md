---
id: TASK-429
title: >-
  The guide's one-writer invariant cites bucket mutations TASK-407 removed
  (NoFence, NoPins*, NoForksAfterLease, NoContinuity): main's guide job fails
status: Done
assignee:
  - '@claude'
created_date: '2026-11-15 09:26'
updated_date: '2026-11-15 10:00'
labels:
  - bug
  - guide
dependencies: []
priority: high
ordinal: 820000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Main's CI guide job fails since 1875ee2a2 (TASK-407: keys written once, commits published by swapping the branch's head), e.g. runs on 7edc54ef3 and 1fd0855fb: guide/tools/check.ts says 'invariant one-writer: no such mutation' for bucket NoFence, NoPinsFences, NoPinsOldFences, NoPinsVoidStrays, NoPinsLateStrays, NoPinsTakings, NoForksAfterLease, NoContinuity. guide/src/content/invariants.ts's one-writer card describes the create-as-fence design (TASK-371/388) that TASK-407 replaced with a head swap (specs/bucket/mutations now has NoHeadSwap, NoEpochKeys, NoTokens, ...). The card's claim, code anchors and evidence must follow the new design, not just new names.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Fixed in 264cc3a37: the one-writer card's claim and why rewritten for the head (a commit is the branch's once the writer's conditional swap of the head names it; a takeover swaps it to the next epoch; the old writer's keys name its epoch); evidence NoHeadSwap, NoEpochKeys, NoTokens, NoCollectorEpochs; anchors acknowledge's note and a new head_swap (head.rs 'pub async fn swap('); the bucket layer's role text. guide: pnpm generate, node tools/check.ts ('63 anchors, 12 invariants, 11 traces: all match the repo'), tsc clean.

2026-11-15, verified independently on the Mac (a second session, on 2fe3a766b): pnpm install --frozen-lockfile, pnpm build (63 anchors, 12 invariants, 11 traces: all match), pnpm test (42 passed). The cited mutations run in TLC (scripts/check-tla.sh): NoHeadSwap violates NoAckedLost, NoEpochKeys KeysWrittenOnce, NoTokens HeadsCurrent, NoCollectorEpochs NothingNeededGone, each in 0-1 s. The guide's evidence kinds cover no Rust tests (sim takeovers_need_head_swaps, collector writers_taken_over_acknowledging_nothing_need_head_swap) and no crates/*/mutants. doc-7 has no section on the head yet (TASK-407.5): the card cites it whole.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Fixed (see notes); verified: ubu1 480962251: workspace 2786/2787 (TASK-315's ZFS page-cache test alone); macOS gate, go and scripts parts passing.
<!-- SECTION:FINAL_SUMMARY:END -->
