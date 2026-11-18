---
id: TASK-444.3
title: >-
  Kernel review agents on guest/linux/fs/cladefs and the fs/dax.c patches, gated
  on reproducers
status: To Do
assignee: []
created_date: '2026-11-15 12:33'
updated_date: '2026-11-17 01:09'
labels: []
dependencies: []
references:
  - >-
    backlog/docs/doc-17 -
    Spending-frontier-tokens-on-quality-what-the-forefront-does-and-what-fits-fxdax.md
parent_task_id: TASK-444
priority: high
ordinal: 852000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Linux kernel AI review has standardised on Chris Mason review-prompts (MIT) and Sashiko (Apache-2.0, Claude Code backend supported): ~85% true findings, ~97% on critical ones, at LSF/MM 2026; Darrick Wong posted 12+ series of LLM-found pre-existing XFS bugs in 2026. Without a reproducer gate, netdev maintainers spend ~50% of their time disproving reviews. Mason dax.md is pmem-only; cladefs writable DAX on write-once extents needs its own context. Runs vary: aggregate several.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 An fxfs/DAX context file states cladefs invariants (doc-4, doc-9, doc-13, specs/dax-cow, dead-ends) in the review-prompts subsystem format
- [ ] #2 A known-issues file lists accepted risks so they stop being reported
- [ ] #3 A sweep of guest/linux/fs/cladefs and the fs/dax.c patches for pre-existing bugs ran 3-5 times; findings recurring or confirmed by a verifier agent are recorded
- [ ] #4 A finding enters the backlog only with a guest/sim seed, a scenario or a guest case that fails
- [ ] #5 A script runs the review on a commit range, documented for use on every change
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
