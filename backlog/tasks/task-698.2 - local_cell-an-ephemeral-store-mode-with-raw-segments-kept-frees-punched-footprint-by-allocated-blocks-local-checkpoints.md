---
id: TASK-698.2
title: >-
  local_cell: an ephemeral store mode with raw segments kept, frees punched,
  footprint by allocated blocks, local checkpoints
status: To Do
assignee: []
created_date: '2026-11-20 16:21'
labels: []
dependencies: []
documentation:
  - >-
    backlog/docs/doc-25 -
    Ephemeral-cells-host-local-children-of-a-version-migrated-live.md
parent_task_id: TASK-698
ordinal: 1091000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
An ephemeral cell has no bucket. Its segments stay raw in its directory, no uploader, framing or lag bound runs, and freed space must come back without compaction. It must recover after a crash from a local checkpoint plus the logs after it (doc-25, The cell).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 An ephemeral store commits, closes segments and reads its own extents with no bucket configured, and starts no uploader or framing
- [ ] #2 A free that is Local punches its extent bytes out of the raw segment; a segment emptied is unlinked; a test shows st_blocks shrinking
- [ ] #3 Its footprint and admission count allocated blocks, so a punched hole returns space to the tenant allotment
- [ ] #4 Recovery from the latest local checkpoint plus later logs gives every Local commit, proven by crash tests (shuttle/madsim) at every step of checkpointing
- [ ] #5 A drain writes a local checkpoint of every ephemeral cell
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
