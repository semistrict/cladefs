---
id: TASK-441.3
title: >-
  Nightly hunts as ordinary work, one a night by weekday: bucket soak, short
  sandboxes, drain rehearsal, Fxfs fuzz targets
status: To Do
assignee: []
created_date: '2026-11-15 11:46'
labels:
  - testing
  - ubu1
dependencies:
  - TASK-439
parent_task_id: TASK-441
priority: medium
ordinal: 833000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
What finds production problems without needing a quiet host: scripts/qualify.sh's bucket soak and short sandboxes (run by hand today), the drain rehearsal (scripts/migrate-host.sh, TASK-439: scheduled nowhere) and crates/fxfs/fuzz's seven cargo-fuzz targets (run in no workflow). They run as plain fxjob work so the bench slot stays free, one a night so ubu1 is not held; their checks must fail for the code, not for the host's load (TASK-389 is that kind of false failure).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each weekday's hunt runs as fxjob work after the night's measurement, and a failure files a task with its replay command, as the measurement does
- [ ] #2 The fuzz targets run from corpora kept between nights, and a crash's input is kept in the results
- [ ] #3 TASK-439's rehearsal is one of the hunts
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
