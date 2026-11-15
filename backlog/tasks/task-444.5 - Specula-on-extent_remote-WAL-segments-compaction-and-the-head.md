---
id: TASK-444.5
title: 'Specula on extent_remote: WAL segments, compaction and the head'
status: To Do
assignee: []
created_date: '2026-11-15 12:33'
labels: []
dependencies: []
references:
  - >-
    backlog/docs/doc-17 -
    Spending-frontier-tokens-on-quality-what-the-forefront-does-and-what-fits-fxdax.md
parent_task_id: TASK-444
ordinal: 854000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Specula (UIUC, Apache-2.0) has agents mine invariants with citations from code, comments, issues and history, write or import TLA+ models, trace-validate, and replay each violation as a code-level test; 0 false positives on 5 systems, median $57 a system. Invariants mined from the backlog, dead-ends and git history may be ones no spec here states.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Specula ran on extent_remote with specs/bucket imported, in its own worktree
- [ ] #2 Each claimed bug has a replaying test and a task, or the notes say why it is not a bug
- [ ] #3 Invariants it found that specs/bucket lacks are added to the spec or argued away in the notes
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
