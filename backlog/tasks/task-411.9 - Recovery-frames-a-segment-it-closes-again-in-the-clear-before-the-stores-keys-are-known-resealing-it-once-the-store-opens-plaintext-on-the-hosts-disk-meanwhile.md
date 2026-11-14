---
id: TASK-411.9
title: >-
  Recovery frames a segment it closes again in the clear before the store's keys
  are known, resealing it once the store opens: plaintext on the host's disk
  meanwhile
status: To Do
assignee: []
created_date: '2026-11-14 23:22'
labels: []
dependencies:
  - TASK-411.3
parent_task_id: TASK-411
priority: high
type: bug
ordinal: 807000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-411.3: WAL recovery runs before the store's ring of keys is built, so a segment it closes again is framed in the clear and resealed (Dir::replace) once the store opens. Until then, and for good if the host dies between, a sealed branch's data sits on local disk in the clear, which shredding (doc-16 goal 2) must not leave. Found by TASK-411.3's implementer.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 No framed file of a sealed branch is ever written in the clear: recovery has the store's keys before it frames, or frames nothing until it has them
- [ ] #2 A test crashes a sealed store with a segment to close again and finds no plaintext of its data in any file of the store's directory at any point of its reopening
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
