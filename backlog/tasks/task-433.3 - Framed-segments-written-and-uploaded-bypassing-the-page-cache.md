---
id: TASK-433.3
title: Framed segments written and uploaded bypassing the page cache
status: To Do
assignee: []
created_date: '2026-11-15 10:27'
labels: []
dependencies: []
parent_task_id: TASK-433
ordinal: 826000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
A framing writes its file through the page cache, written back a piece at a time, and the uploader reads it back from there (TASK-401's read_object). Write it with O_DIRECT; the upload reads it from disk, or from what the framing holds in memory.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
