---
id: TASK-444.8
title: >-
  Assumptions as assertions on hostile and lifetime paths, then fuzzed and
  hunted
status: To Do
assignee: []
created_date: '2026-11-15 12:34'
labels: []
dependencies: []
references:
  - >-
    backlog/docs/doc-17 -
    Spending-frontier-tokens-on-quality-what-the-forefront-does-and-what-fits-fxdax.md
parent_task_id: TASK-444
ordinal: 857000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-Augur (arXiv 2606.18619): for each site an agent judges safe, write the reason as an assertion and let fuzzers try to break it: 34-157% more bugs than an agent alone. AGENTS.md learned the same from the graveyard reap: to close a narrow race, assert the rule. Theori Branch Flipper: a model reads a branch a fuzzer is stuck on and writes a generator.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 virtio request decoding, the extentd codec, the window and uffd paths and dax.c have their safety assumptions as assertions
- [ ] #2 The fuzz targets, hostile tests, guest/sim and the hunt run with them on; each trip is fixed or the assumption corrected
- [ ] #3 Coverage plateaus in the cargo-fuzz targets get structure-aware generators, with coverage before and after recorded
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
