---
id: TASK-420
title: >-
  Flaky: nextest flags Shuttle tests leaky now and then on macOS (follower
  busy_by_pct, extent_store needs_closes_to_wait_for_framings)
status: To Do
assignee: []
created_date: '2026-11-15 02:08'
labels:
  - testing
  - flaky
dependencies: []
priority: low
type: bug
ordinal: 808000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-400's runs of scripts/test.sh's shuttle part on macOS (2026-11-15): two runs each flagged one Shuttle test LEAK though it passed: follower::shuttle busy_by_pct (0.69 s), then extent_store::shuttle needs_closes_to_wait_for_framings (0.23 s), a different test each run; the second passed 5/5 without the flag rerun alone. Under the shuttle feature the store starts no thread of its own outside Shuttle's schedule (TASK-400's packing ahead packs inline there), so it's likely the harness or nextest's leak heuristic (a child holding the test's stdout/stderr past its exit). Find what holds the handles, or say why it's benign.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The cause of the LEAK flags is found and fixed, or shown harmless and recorded
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
