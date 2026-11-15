---
id: TASK-444.10
title: 'Verus proofs of small pure cores: extent arithmetic and segment framing'
status: To Do
assignee: []
created_date: '2026-11-15 12:43'
labels: []
dependencies: []
references:
  - >-
    backlog/docs/doc-17 -
    Spending-frontier-tokens-on-quality-what-the-forefront-does-and-what-fits-fxdax.md
parent_task_id: TASK-444
priority: low
ordinal: 859000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Kani is excluded (CBMC is BSD-4-Clause, OPEN_QUESTIONS.md). Verus (MIT) is the OSI route to bounded proofs. VeruSAGE (2025-26): agents prove over 80% of proof tasks drawn from real Verus-verified systems, a storage system among them; adding proofs to existing code is far easier than writing verified code from a spec (44% on the vericoding benchmark). Proofs are often vacuous or cheat: 12.4% of AutoVerus candidates assumed their goal, and Dan Luu saw "A implies A" proofs. Ranked below TASK-444.1-9: the code has to be rewritten in Verus syntax, and the model tests and fuzz targets already cover these paths.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 One pure core (extent offset and alignment arithmetic, or segment framing) is proven panic-free and in bounds in Verus
- [ ] #2 Verus runs with --no-cheating and a check that rejects assume and external_body in proof code
- [ ] #3 Each proven property is shown non-vacuous: a mutant of the code makes the proof fail
- [ ] #4 The notes say whether the proof found anything the tests had missed, and whether to extend it
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
