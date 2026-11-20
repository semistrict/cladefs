---
id: TASK-702.5
title: Read-ahead runs only with a part cache
status: To Do
assignee: []
created_date: '2026-11-20 19:47'
labels:
  - performance
  - cache
dependencies: []
parent_task_id: TASK-702
priority: low
ordinal: 1107000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
A fetcher with no part cache keeps nothing, not even the 64 MiB of recently fetched parts (crates/bucket_cell/src/fetch.rs, `read_ahead`, `recent`), so it reads nothing ahead. The sandbox team measured a cold boot without a part cache at 9.1 s against 3.1 s with one, its first command 6.3 s against 3.6 s. They now always run one; other integrators would hit it.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A fetcher with no part cache reads ahead into memory, its parts kept until a read takes them or within the recent-parts bound
- [ ] #2 A test shows a sequential reader with no part cache finding the parts read ahead without a second GET
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
