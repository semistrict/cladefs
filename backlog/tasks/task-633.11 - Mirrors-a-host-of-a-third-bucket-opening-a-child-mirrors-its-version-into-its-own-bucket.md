---
id: TASK-633.11
title: >-
  Mirrors: a host of a third bucket opening a child mirrors its version into its
  own bucket
status: To Do
assignee: []
created_date: '2026-11-21 07:34'
labels:
  - bucket
  - multi-region
dependencies: []
parent_task_id: TASK-633
priority: medium
ordinal: 1127000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found building TASK-633.10's mirror profile (2026-11-21). ObjectBucket::mirror_for, run on every store opening (upload::open), mirrors the child's version into the opening host's bucket whenever the version's cell is of another bucket, without checking that the child is of the host's bucket. A host of bucket C opening a child of B (parent in A) copies the version into C, where no child refers to it: refer_here moves only children of C, so C's copy is kept for as long as the parent lives, for nothing. Fix: mirror::wanted, shared by ObjectBucket::mirror_wanted and the simulation, wants a mirror only for a child of the asking bucket.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 mirror::wanted wants a mirror only when the child is of the asking bucket and the version's cell of another
- [ ] #2 A test in which a third bucket's host asks for the mirror and nothing is requested, caught by a hand mutant
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 scripts/upstream-changes.py --check passes
- [ ] #2 Deviations from upstream documented where they occur
<!-- DOD:END -->
