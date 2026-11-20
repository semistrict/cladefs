---
id: TASK-685.3
title: 'Threads, runtimes and blocking through one seam for the simulation'
status: In Progress
assignee: []
created_date: '2026-11-20 03:32'
updated_date: '2026-11-20 14:36'
labels:
  - testing
  - simulation
dependencies: []
parent_task_id: TASK-685
ordinal: 1069000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-24 step 3: about 100 thread spawn sites, the runtimes cladefsd builds (server, control, peers, bucket, pager), block_in_place: one seam that is a thread or runtime built normally and a madsim task under cfg(madsim); blocking() inline under madsim.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 No std::thread::spawn, runtime build or block_in_place outside the seam in cladefsd's code; a lint refuses new ones
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-20, after the user's decision (the daemon async down to its waits): the plan is doc-24's 'The async rewrite's plan', 12 steps from the inventory of cross-task waits; each step leaves the normal build as it is. Step 1 (fail fast under madsim) and step 2 (async uploader waits) first. Found meanwhile: TASK-688 (a store taken on the control runtime's worker).

2026-11-20: steps 1-4, 6, 8 landed on main by PR #2 (5f65a04ee). The rest goes to main directly (the user: no more PRs). Next batch: step 9 (explicit async close of daemon-owned stores) and step 5 (read path), with TASK-685.12 (madsim build in test.sh/CI).
<!-- SECTION:NOTES:END -->
