---
id: TASK-698.6
title: >-
  Docs: ephemeral cell, decommissioning, hand-over and local checkpoints in
  CONTEXT.md, architecture.md and doc-12
status: Done
assignee: []
created_date: '2026-11-20 16:22'
updated_date: '2026-11-21 00:45'
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
- [x] #1 CONTEXT.md defines Ephemeral cell, Decommission, Hand over, Local checkpoint and the host incarnation, with their _Avoid_ words
- [x] #2 docs/architecture.md and doc-12 say what an ephemeral cell durability and sync mean
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 scripts/upstream-changes.py --check passes
- [x] #2 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
CONTEXT.md gained Ephemeral cell, Hand over, Decommission, local checkpoint and the incarnation note with doc-25 (2026-11-20); what remains is architecture.md and doc-12, and whatever terms the code adds.

2026-11-20: doc-12 has an ephemeral cell table (failures, fsync/sync, tests); docs/architecture.md an Ephemeral cells section; the ephemeral local checkpoint renamed from Base to LocalCheckpoint (file local-checkpoint), base being the glossary word for a store VMs boot forks of.

DoD #1 left open: the touched crates passed on macOS and Linux (local_cell 382/382 macOS, local_cell+bucket_cell 718/718 Linux, cladefs lib+cells/control/peers/handover 542/543 Linux with TASK-700 unrelated, the madsim suite 20 seeds); the whole workspace is CI's, running at close. upstream-changes --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
CONTEXT.md gained Ephemeral cell, Hand over, Decommission, local checkpoint and the incarnation note; doc-12 a table of what an ephemeral cell keeps through each failure, with its tests; docs/architecture.md an Ephemeral cells section; the code says local checkpoint, not base.
<!-- SECTION:FINAL_SUMMARY:END -->
