---
id: TASK-444
title: 'Agents with external oracles: the quality program of doc-17'
status: To Do
assignee: []
created_date: '2026-11-15 12:32'
updated_date: '2026-11-15 13:31'
labels: []
dependencies: []
references:
  - >-
    backlog/docs/doc-17 -
    Spending-frontier-tokens-on-quality-what-the-forefront-does-and-what-fits-fxdax.md
priority: high
ordinal: 849000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The user has a large surplus of frontier-model tokens to spend on quality (2026-11-15). Web research of what the forefront does (doc-17) found one pattern behind every result that worked: a model proposes, something outside the model disposes (ASan, TLC, a fuzzer, a mutant a test must kill, loom). Model-only judgement fails in measured ways (1:50 signal-to-noise in hunts, self-approval, "benign" races, fixes that hide symptoms). cladefs already owns cheap deterministic disposers (guest/sim seeds, Shuttle, the model test oracle, TLC, mutants), so token-hungry hunts are affordable here when gated on them. The subtasks are ranked by expected bugs per effort; numbering is the rank. Shared gate for every subtask: no reproducer in one of the project oracles, no finding.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each subtask's notes say what it found, or that it found nothing
- [ ] #2 doc-17's ranking is corrected when a result contradicts it
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
