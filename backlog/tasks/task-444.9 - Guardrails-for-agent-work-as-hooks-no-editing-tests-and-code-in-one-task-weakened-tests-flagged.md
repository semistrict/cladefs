---
id: TASK-444.9
title: >-
  Guardrails for agent work as hooks: no editing tests and code in one task,
  weakened tests flagged
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 12:34'
updated_date: '2026-11-15 13:34'
labels: []
dependencies: []
references:
  - >-
    backlog/docs/doc-17 -
    Spending-frontier-tokens-on-quality-what-the-forefront-does-and-what-fits-fxdax.md
parent_task_id: TASK-444
ordinal: 858000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
ImpossibleBench (ICLR 2026): frontier models cheat on contradictory tests, stronger ones more; Anthropic guidance: enforce rules with hooks, not instruction lines. AGENTS.md forbids tests written around bugs but nothing enforces it. Dan Luu: reviewer agents that edit code made results worse; keep them report-only.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A hook denies implementer agents edits to tests, specs and mutation configs unless the task is a test-writing one
- [ ] #2 A check flags diffs that remove assertions, add #[ignore], loosen bounds or narrow generators, run before commit
- [ ] #3 A read-only, report-only review step verifies each finding before reporting, with a second vendor on load-bearing changes
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
