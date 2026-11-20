---
id: TASK-698.6
title: >-
  Docs: ephemeral cell, decommissioning, hand-over and local checkpoints in
  CONTEXT.md, architecture.md and doc-12
status: To Do
assignee: []
created_date: '2026-11-20 16:22'
updated_date: '2026-11-20 19:49'
labels: []
dependencies: []
documentation:
  - >-
    backlog/docs/doc-25 -
    Ephemeral-cells-host-local-children-of-a-version-migrated-live.md
parent_task_id: TASK-698
ordinal: 1095000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The glossary binds code identifiers; doc-25 adds terms that must be defined before code uses them.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 CONTEXT.md defines Ephemeral cell, Decommission, Hand over, Local checkpoint and the host incarnation, with their _Avoid_ words
- [ ] #2 docs/architecture.md and doc-12 say what an ephemeral cell durability and sync mean
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
CONTEXT.md gained Ephemeral cell, Hand over, Decommission, local checkpoint and the incarnation note with doc-25 (2026-11-20); what remains is architecture.md and doc-12, and whatever terms the code adds.

2026-11-20: doc-12 has an ephemeral cell table (failures, fsync/sync, tests); docs/architecture.md an Ephemeral cells section; the ephemeral local checkpoint renamed from Base to LocalCheckpoint (file local-checkpoint), base being the glossary word for a store VMs boot forks of.
<!-- SECTION:NOTES:END -->
