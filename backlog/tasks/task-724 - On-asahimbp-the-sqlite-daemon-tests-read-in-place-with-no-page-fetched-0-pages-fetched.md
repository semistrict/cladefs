---
id: TASK-724
title: >-
  On asahimbp the sqlite daemon tests read in place with no page fetched: 0
  pages fetched
status: To Do
assignee: []
created_date: '2026-11-21 07:39'
updated_date: '2026-11-21 07:39'
labels:
  - bug
  - arm64
dependencies: []
priority: medium
ordinal: 1129000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen 2026-11-21 on asahimbp's podman image, on the pre-async base 1b3b5a991 too. sqlite_extents::daemon a_database_on_the_daemon_reads_in_place_and_survives_reopening and its encrypted twin fail at tests/daemon.rs:125, assert file_set.fetched() > before + 100: '0 pages fetched'. Reads that should fetch pages through the daemon fetch none there: 16K pages, or the container. Passes on ubu1.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 scripts/upstream-changes.py --check passes
- [ ] #2 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Correction: 'Passes on ubu1' is unverified as of filing; not run there for this task.
<!-- SECTION:NOTES:END -->
