---
id: TASK-444.2.2
title: >-
  A follower's guest may miss a writer's commit made as its device starts
  hearing of commits
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 13:48'
updated_date: '2026-11-15 14:01'
labels: []
dependencies: []
parent_task_id: TASK-444.2
priority: medium
type: bug
ordinal: 862000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-444.2's loom model hearing::loom::listeners_hear_of_every_rise (2026-11-15), on the code as it was. virtio_extent's hear_commits set the store's on_committed listener (OnceLock, releasing) then read RemoteExtentStore::committed (an acquiring load); the client's receiving thread raised committed (fetch_max AcqRel) then read the listener (an acquiring load). Store buffering: both reads may miss, so a commit told as the device starts reaches neither the listener nor the catch-up read, and the follower's guest is told of it only at the writer's next commit, never if the writer stops there. Loom: heard 0 of a rise to 5. The API had no way to listen without that gap: listening and reading the latest were two calls.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A loom model (rise vs listen) shows every rise heard, from the listener or from what listening returns, under every interleaving and ordering
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
Found: loom model hearing::loom::listeners_hear_of_every_rise on the logic as it was (hearing::Latest extracted unchanged: rise = fetch_max AcqRel then listener.get(); the device's listen then latest()): 'the rise to 5 unheard: left 0, right 5'.
Blind spot: as TASK-444.2.1: Shuttle's SeqCst, and no test committed as a follower's device set its listener (tests listen first, then commit).
Class test: the model checks that every rise is heard from the listener or from what listening returns. Fix: Latest::listen returns the latest through fetch_max(0, Release), an RMW ordered against the rise's; RemoteExtentStore::on_committed returns it; virtio_extent's hear_commits passes it to Following::committed (a fetch_max, so a rise heard twice is harmless). Bug back: crates/fxdax/loom-mutants/latest-listening-loads.patch, killed; latest-rises-relaxed, latest-listens-relaxed, killed.
Look around and structure: as TASK-444.2.1's notes (same owner, hearing; TASK-444.2.4 filed for on_capacity's gap).
<!-- SECTION:NOTES:END -->
