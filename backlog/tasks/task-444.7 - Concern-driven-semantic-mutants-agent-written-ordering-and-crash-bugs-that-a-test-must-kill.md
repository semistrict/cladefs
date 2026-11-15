---
id: TASK-444.7
title: >-
  Concern-driven semantic mutants: agent-written ordering and crash bugs that a
  test must kill
status: To Do
assignee: []
created_date: '2026-11-15 12:34'
updated_date: '2026-11-15 13:32'
labels: []
dependencies: []
references:
  - >-
    backlog/docs/doc-17 -
    Spending-frontier-tokens-on-quality-what-the-forefront-does-and-what-fits-fxdax.md
parent_task_id: TASK-444
ordinal: 856000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
cargo-mutants changes operators; cladefs real bugs are orderings and lifetimes (truncate before fsync, publish before seal, a missing fence, a lost wakeup). Meta ACH (FSE 2025) has a model write realistic mutants from prose concerns and tests that kill survivors (73% of tests accepted); an LLM equivalent-mutant filter reaches ~0.95 precision.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A list of fault concerns drawn from the backlog bug classes is recorded
- [ ] #2 Agent-written mutants for each concern, with equivalent ones filtered out, are run against the suites
- [ ] #3 Each surviving mutant gets a killing test or a task under TASK-149
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
