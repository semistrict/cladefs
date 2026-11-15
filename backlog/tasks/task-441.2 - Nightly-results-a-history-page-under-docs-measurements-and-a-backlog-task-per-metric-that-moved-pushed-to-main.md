---
id: TASK-441.2
title: >-
  Nightly results: a history page under docs/measurements and a backlog task per
  metric that moved, pushed to main
status: To Do
assignee: []
created_date: '2026-11-15 11:46'
labels:
  - testing
  - ubu1
dependencies: []
parent_task_id: TASK-441
priority: high
ordinal: 832000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The user reads results as backlog tasks (2026-11-15), and nothing on a quiet night. cron on ubu1 has no agent in it, so the script itself commits the night's results and files tasks with the backlog CLI, in a worktree of its own, and pushes to main (the user's standing rule for verified work). A regression already filed and still open must not be filed again each night.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A night with a moved metric leaves one commit on main: the results, the history page, and one task naming the metric, both commits, the rounds' numbers and the command that replays it
- [ ] #2 The same regression on the following nights adds a note to its open task, not a new task
- [ ] #3 A night that could not run or whose rounds were all discarded says so in the history page, and three such nights in a row file a task
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
