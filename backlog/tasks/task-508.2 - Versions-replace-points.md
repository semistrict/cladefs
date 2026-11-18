---
id: TASK-508.2
title: Versions are the only named past states
status: To Do
assignee: []
created_date: '2026-11-18 00:10'
updated_date: '2026-11-18 00:24'
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
Two mechanisms name a past state today: a version, kept inside its cell (doc-18), and a frozen log of its own that hosts fork (the code's points: Record::point, ForkStore's point flag, SharePoint). doc-20 has forks made only from versions, so the second goes. Making a version must stay local: one commit closing the segment, no bucket round trip.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Forks are made only from versions, on the version's host locally or elsewhere once it is uploaded
- [ ] #2 Dropping a version with children keeps its data until they go
- [ ] #3 Shared versions replace the code's shared points; nothing of the second mechanism is left
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
