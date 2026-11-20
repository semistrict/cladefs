---
id: TASK-685.11
title: 'Compare the full-stack simulation with FoundationDB''s again, once it runs'
status: To Do
assignee: []
created_date: '2026-11-20 08:00'
labels:
  - testing
  - simulation
dependencies: []
parent_task_id: TASK-685
ordinal: 1080000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The user, 2026-11-20: look at FoundationDB's simulation testing now (doc-24, What FoundationDB does that we don't) and again at the end of the process, to compare. Once the full-stack simulation runs its fault profiles (TASK-685.7) and coverage (TASK-685.10): which of doc-24's ten adoptions were made, what each found, and what FoundationDB does still that we don't.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 doc-24 compares the simulation as built with FoundationDB's: each adoption made or not, with why, and what it found
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
