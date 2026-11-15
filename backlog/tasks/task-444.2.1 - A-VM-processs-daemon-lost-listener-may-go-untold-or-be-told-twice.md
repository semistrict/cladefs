---
id: TASK-444.2.1
title: 'A VM process''s daemon-lost listener may go untold, or be told twice'
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 13:48'
updated_date: '2026-11-15 14:01'
labels: []
dependencies: []
parent_task_id: TASK-444.2
priority: high
type: bug
ordinal: 861000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-444.2's loom model hearing::loom::occurrences_are_told_once (2026-11-15), on the code as it was. extentd client: the receiving thread stored lost=true (Release) then read on_lost (OnceLock, an acquiring load); DaemonConnection::on_lost set on_lost (OnceLock, releasing) then loaded lost (Acquire). That is store buffering: each side's load may miss the other's store, so neither calls the listener, and the VMM, whose listener exits the process (vendor/firecracker extent device, TASK-50.2, TASK-232), keeps its VM running with every request failing. Loom: told 0 times. Under SeqCst alone both sides may also see each other and both call it: two threads calling std::process::exit at once. Shuttle could not see the first (it makes every atomic SeqCst); no test ran the second interleaving.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A loom model of the protocol (lost vs listen) shows exactly one call under every interleaving and ordering
- [ ] #2 The bug put back (a loom-mutants patch) turns the model red
- [ ] #3 Notes record the blind spot, the class test, what a look around found, and the structure question
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Found: loom model hearing::loom::occurrences_are_told_once on the logic as extentd's client had it, extracted unchanged into hearing::Occurrence (store lost Release, then on_lost.get(); on_lost.set(), then lost.load(Acquire)): 'told so many times: left 0, right 1'. The double call (both sides see each other) is an SC interleaving, also possible.
Blind spot: Shuttle runs every atomic as SeqCst, so the 0-call outcome was invisible to it; and no test raced listening against the daemon going (tests register on_lost long before killing the daemon).
Class test: the model checks exactly one call under every interleaving and ordering, for any news-plus-listener handoff built on hearing::Occurrence. Bug back: crates/fxdax/loom-mutants/occurrence-told-by-a-store-and-a-load.patch, killed. Each of the fix's two orderings weakened: occurrence-happens-relaxed, occurrence-listens-relaxed, killed.
Look around: the same shape in the commit handoff (TASK-444.2.2, found and fixed alongside); on_capacity drops news before its listener (TASK-444.2.4, a plain race, unreproduced); on_stats_asked drops likewise but answers itself, as documented; physmem's QUARANTINE OnceLock is set at the first connection, before any window stages a page; event_listener's listen-then-check (InFlight::take) is safe as 5.4.2's notify always takes the list lock (docs/memory-ordering.md).
Structure: the class is now impossible where it is used: hearing owns each handoff, the news and the listener meet in one RMW of one atomic, and the API has no store-then-load to get wrong. Nothing architectural.
<!-- SECTION:NOTES:END -->
