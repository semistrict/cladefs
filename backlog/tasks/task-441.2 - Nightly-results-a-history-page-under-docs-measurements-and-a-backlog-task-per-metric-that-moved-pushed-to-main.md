---
id: TASK-441.2
title: >-
  Nightly results: a history page under docs/measurements and a backlog task per
  metric that moved, pushed to main
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 11:46'
updated_date: '2026-11-15 23:40'
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
- [x] #1 A night with a moved metric leaves one commit on main: the results, the history page, and one task naming the metric, both commits, the rounds' numbers and the command that replays it
- [x] #2 The same regression on the following nights adds a note to its open task, not a new task
- [x] #3 A night that could not run or whose rounds were all discarded says so in the history page, and three such nights in a row file a task
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. scripts/nightly.py: plan (skip when origin/main is the newest night's head; baseline = newest measured night's head, first night HEAD's parent), report (verdicts.json and nightly.md in the night's results, docs/measurements/ubu1-nightly-DATE-HEAD), history page docs/measurements/nightly-history.md regenerated from every night's verdicts.json, one backlog task per moved metric keyed [nightly:KEY] in its title (open one gets a note instead), three bad nights in a row file one task (keyed too), commit and push to main with rebase retries, never force; a failed push logged under ~/.local/state/cladefs/nightly for the next night, which pushes it first.
2. scripts/nightly-host.sh: cron's entry: lock, fetch, update its own clone (~/src/cladefs-nightly) to origin/main and re-exec, plan, bench-ab-host.sh with CLADEFS_HOST=local (build as work, rounds in fxjob --bench), report, publish.
3. scripts/nightly-install.sh: the crontab line (02:00, user ramon) a system-setup.d step calls; README's testing section.
4. Tests end to end on fake tools (fxjob, sudo, backlog, a bare origin): skip, task filing and note, three bad nights, push retry on a moved main, failed push kept and pushed next night.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
scripts/nightly.py (plan/report/history/sync/publish), scripts/nightly-host.sh (cron entry: lock, sync + re-exec main's copy, plan, bench-ab-host.sh local, report, publish), scripts/nightly-install.sh (clone, identity, crontab at 02:00, prerequisites checked). Main moved = newest origin/main commit changing more than docs/measurements and backlog (the nightly's own commits, and backlog-only ones, are no change). Base = newest night whose head built, else the first night's base, else head^. Night records docs/measurements/HOST-nightly-<time>-<head8>/night.json + nightly.md + rounds.json; nightly-history.md regenerated from them. Tasks keyed [nightly:CASE NAME VERDICT] in the title, labels perf,nightly, priority high; an open one (status not Done) gets a dated note; three bad nights in a row file [nightly:no-result]. Push: 3 tries, fetch+rebase between, never force; rebase conflict sets the commits aside on nightly-unpushed-<time>; failure in STATE/push-failed.txt, pushed by the next night's sync and repeated in its report. Tests: test_nightly.py (10, end to end on fake fxjob/sudo/backlog and a bare origin). README's testing section: 'The nightly measurement on ubu1'. Decisions in OPEN_QUESTIONS.md.

AC #1-#3 checked on test_nightly.py: nightly-host.sh end to end on this machine as its local host, a bare origin, fake fxjob/sudo/backlog and fixture rounds: one commit on main with the results, history page and one task (title, both commits, rounds' numbers, replay command); the next night's note on the open task (a Done one filed again); could-not-run and no-result nights in the history page, three in a row one [nightly:no-result] task. Open: DoD #1, and the first real night on ubu1 with backlog's real CLI and a real push (TASK-441's ACs). Prerequisites on ubu1 nightly-install.sh checks: fxjob, flock, cargo-nextest, backlog (Backlog.md CLI, MIT; not known to be installed there), a prompt-less fetch and push from cron.
<!-- SECTION:NOTES:END -->
