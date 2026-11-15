---
id: TASK-429
title: >-
  The guide's one-writer invariant cites bucket mutations TASK-407 removed
  (NoFence, NoPins*, NoForksAfterLease, NoContinuity): main's guide job fails
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 09:26'
updated_date: '2026-11-15 09:44'
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
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Fixed in 264cc3a37: the one-writer card's claim and why rewritten for the head (a commit is the branch's once the writer's conditional swap of the head names it; a takeover swaps it to the next epoch; the old writer's keys name its epoch); evidence NoHeadSwap, NoEpochKeys, NoTokens, NoCollectorEpochs; anchors acknowledge's note and a new head_swap (head.rs 'pub async fn swap('); the bucket layer's role text. guide: pnpm generate, node tools/check.ts ('63 anchors, 12 invariants, 11 traces: all match the repo'), tsc clean.
<!-- SECTION:NOTES:END -->
