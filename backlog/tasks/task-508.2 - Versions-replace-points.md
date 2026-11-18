---
id: TASK-508.2
title: Versions replace points
status: To Do
assignee: []
created_date: '2026-11-18 00:10'
labels:
  - bucket
  - host
dependencies: []
documentation:
  - doc-20
parent_task_id: TASK-508
ordinal: 899000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
A point is a frozen branch of its own that hosts fork; a version is a cell's past state kept inside it (doc-18). With forks only from versions (doc-20), points are a second way to name a past state. Making a version must stay local: one commit closing the segment, no bucket round trip.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Forks are made only from versions, on the version's host locally or elsewhere once it is uploaded
- [ ] #2 A shared point is a shared version; no point code is left
- [ ] #3 Dropping a version with children keeps its data until they go
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
