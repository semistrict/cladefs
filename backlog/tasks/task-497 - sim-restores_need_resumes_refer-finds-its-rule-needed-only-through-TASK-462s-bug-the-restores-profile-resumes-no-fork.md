---
id: TASK-497
title: >-
  sim restores_need_resumes_refer finds its rule needed only through TASK-462's
  bug: the restores profile resumes no fork
status: To Do
assignee: []
created_date: '2026-11-16 16:11'
labels:
  - sim
  - test
dependencies: []
priority: high
ordinal: 887000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
On main (d507f8cd6) the test passes at seed 2655, whose violation, 'collector: lost: segment 1 of 1/2/2, which 1/48/48 lives on', is the same with every rule on: TASK-462's lost referenced fork point, fixed on t3/explore-tenant-encryption-keys (4ab4e745e). Counted over seeds 0..20000 of the restores profile (a probe in branch::resume_point_fork), no store resumes a fork being made, on main or the branch: a restore whose host crashed is set aside for deletion (World::fail_restore) and never opened, so the resume path, and the rule resumes_refer in it, is never reached. With TASK-462 fixed, the test finds no seed (no seed of 0..20000 needed the rule). The rule's direct test, extent_bucket's clones_resumed_need_resumes_refer, passes and does fail without it.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The simulation opens a restore's child while its fork is being made, its host dead or slow, as a store opening it does (upload::resumed), and checks the clone it resumes
- [ ] #2 restores_need_resumes_refer finds a violation of the rule's own (not TASK-462's), named in the test
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
