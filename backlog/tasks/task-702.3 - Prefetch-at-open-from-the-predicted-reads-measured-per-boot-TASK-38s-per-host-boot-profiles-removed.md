---
id: TASK-702.3
title: >-
  Prefetch at open from the predicted reads, measured per boot; TASK-38's
  per-host boot profiles removed
status: To Do
assignee: []
created_date: '2026-11-20 19:46'
labels:
  - sandbox
  - performance
  - cache
dependencies:
  - TASK-702.1
  - TASK-702.2
parent_task_id: TASK-702
priority: high
ordinal: 1105000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
A cold host's first fork of a version reads its base a part at a time (TASK-670: ~1.6 GETs in flight). With a prediction of its reads from recent evidence (the model TASK-702.1 chooses), the daemon can fetch the predicted parts at once as the fork opens, behind demand reads, so the guest boots at once and finds them local. The part cache suffices: the sandbox team measured a base on local disk alone as fast as one in memory (0.20 s), and it charges no tenant's arena. Measuring each boot's prediction makes drift visible and lets the next model correct it.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 When a fork of a version opens, the daemon fetches the predicted parts into the part cache through the host fetcher as read-ahead: demand reads go first, peers are asked first, under the fetch budget, many in flight; the fork never waits for it; a test shows demand ahead of prefetch and more than four parts in flight
- [ ] #2 Only extents the fork's own store holds are prefetched
- [ ] #3 Each boot's prediction is measured against its reads (parts predicted and read, predicted and unread, read and unpredicted), in stats
- [ ] #4 TASK-38's boot profiles (profile files, their recorder and replay threads) are gone
- [ ] #5 On the test host, a cold host's first boot of a version with evidence answers within 1.5x of a warm one, with cold and with warm peers, recorded in docs/measurements
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
