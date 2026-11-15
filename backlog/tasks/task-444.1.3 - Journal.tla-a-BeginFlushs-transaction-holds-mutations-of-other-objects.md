---
id: TASK-444.1.3
title: 'Journal.tla: a BeginFlush''s transaction holds mutations of other objects'
status: To Do
assignee: []
created_date: '2026-11-15 14:25'
labels:
  - spec
dependencies: []
parent_task_id: TASK-444.1
ordinal: 863000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Spec divergence (TASK-444.1), spec side. The revised spec had a flush's BeginFlush in a transaction of its own; the allocator's (allocator.rs, start) creates its layer's object in the root store in the same transaction. The trace of model_tests::journal_trace's run-0000 (Setting::awkward(5713515047801504255, ROOMY).cutting_everywhere(), the store flushed around a super-block) stopped at step 13, Commit m={o4} b=o5. Fixed in the spec: BeginFlush(o, S), with the rule the code's comment states (beginFlushApart: S holds no mutation of o; the mutation NoBeginFlushApart shows the loss it prevents, a mutation in the BeginFlush's transaction that replay skips with it, Journal::replay's offset + 1, now in the spec's ReplayFromAt). Replaying test: scripts/check-tla.sh journal/traces.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
