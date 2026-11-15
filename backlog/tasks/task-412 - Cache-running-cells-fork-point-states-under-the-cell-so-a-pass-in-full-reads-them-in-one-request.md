---
id: TASK-412
title: >-
  Cache running cells' fork-point states under the cell, so a pass in full reads
  them in one request
status: To Do
assignee: []
created_date: '2026-11-14 19:00'
labels:
  - bucket
  - collector
dependencies: []
priority: low
ordinal: 799000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Moved from TASK-395.12 so TASK-395 can close (user, 2026-11-14: keep for later; build once running VMs commonly keep dozens of live snapshots, e.g. agent sandboxes snapshotting every step). Measured there (docs/measurements/macos-gc-fork-points-2026-11-14, after TASK-395.18/.19): a pass over a 164k-extent cell takes 0.24 / 0.44 / 2.9 s at 1 / 10 / 100 fork points, about 1 + P/9 times the one-point pass; decision in OPEN_QUESTIONS.md (TASK-395.12).

TASK-395.6 skips cells unchanged since their last pass, but a cell that is written to (a running VM) goes in full every pass, and recovers the state at every fork point its references name: one GET of the checkpoint at each fork point for forks across cells (a VM snapshotted N times reads N checkpoints, each a disk's worth of extents, every pass). TASK-395.6 decided not to cache fork-point states (OPEN_QUESTIONS.md, Decisions, TASK-395.6): a cache of each state holds as many bytes as the checkpoints it replaces, and a cached state goes stale when the parent gets a checkpoint at or before the fork point after it was cached (record_fork writes one at the fork point if missing): a pass going by the cache would keep the older checkpoint and segments and delete the new checkpoint, which the child's recovery reads by its key, losing the child. What would pay: one object under <C>gc/ holding the union of the fork points' located extents and touched keys (snapshots of one disk share most), with, per fork point, the checkpoint its recovery started from and the in-cell keys it read, valid only while the listing shows the same newest checkpoint at or before each fork point and every in-cell key it read; else recover them anew. Measure first: whether such cells' passes cost enough to matter.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A pass in full over a cell whose fork points are unchanged reads their states in one request, not one per fork point, shown by a request-counting test
- [ ] #2 A checkpoint of the parent written at or before a fork point after its state was cached makes the next pass recover that fork point anew and keep the new checkpoint (a test that goes red with the check removed)
- [ ] #3 A fork point added or retired makes the next pass recover the fork points anew, so that a retired one's state goes
- [ ] #4 Bucket.tla models the cache, with a mutation (no validation) TLC fails
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
