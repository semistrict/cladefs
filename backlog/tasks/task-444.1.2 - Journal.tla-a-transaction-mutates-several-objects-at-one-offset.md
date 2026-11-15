---
id: TASK-444.1.2
title: 'Journal.tla: a transaction mutates several objects at one offset'
status: To Do
assignee: []
created_date: '2026-11-15 14:24'
labels:
  - spec
dependencies: []
parent_task_id: TASK-444.1
ordinal: 862000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Spec divergence (TASK-444.1), spec side. specs/journal had one record a mutation, each at its own offset, and an object's offset the offset of its own record; the code commits a transaction of several objects' mutations at one offset (write_and_apply_mutations: checkpoint_before for all), durable whole or not at all, and each object's checkpoint is the transaction's. A trace of any sequence (its first commit mutates the root store and the allocator together) matches no behaviour of the old spec. Fixed in the spec: a record is a transaction (Commit(S), BeginFlush(o, S), EndFlush(o, S)); TLC and every mutation pass. Replaying test: model_tests::journal_trace::journal_traces_follow_the_spec's traces, checked by scripts/check-tla.sh journal/traces.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
