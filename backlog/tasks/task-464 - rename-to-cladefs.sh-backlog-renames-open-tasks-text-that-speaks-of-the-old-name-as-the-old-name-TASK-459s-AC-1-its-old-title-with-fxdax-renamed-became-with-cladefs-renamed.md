---
id: TASK-464
title: >-
  rename-to-cladefs.sh --backlog renames open tasks' text that speaks of the old
  name as the old name: TASK-459's AC #1 'its old title with fxdax renamed'
  became 'with cladefs renamed'
status: To Do
assignee: []
created_date: '2026-11-15 19:14'
labels:
  - backlog
  - scripts
dependencies: []
ordinal: 871000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Running scripts/rename-to-cladefs.sh --backlog again (2026-11-15, on the encryption-at-rest branch after merging its TASK-411.28 work, renamed) rewrote TASK-459's acceptance criterion #1, which names fxdax as the old name, to name cladefs, reversing its meaning; its updated_date changed too. The edit was reverted by hand before committing. The script renames every open task's title, description, ACs and plan, with no way to keep a task's words that mean the old name. Either keep words like 'fxdax renamed' / 'old name fxdax' (a keep pattern), or skip tasks that are about the rename itself (TASK-437 is skipped by ID already; TASK-459 is its sibling), or document that --backlog is meant to run once per branch and say so when run on a backlog already renamed.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 rename-to-cladefs.sh --backlog run on a backlog already renamed changes no task about the rename (TASK-437, TASK-459)
- [ ] #2 A test of the script shows TASK-459-like text kept
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
