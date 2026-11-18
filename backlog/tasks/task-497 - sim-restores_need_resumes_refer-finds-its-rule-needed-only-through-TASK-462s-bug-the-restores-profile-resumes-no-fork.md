---
id: TASK-497
title: >-
  sim restores_need_resumes_refer finds its rule needed only through TASK-462's
  bug: the restores profile resumes no fork
status: To Do
assignee: []
created_date: '2026-11-16 16:11'
updated_date: '2026-11-18 01:22'
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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-17 tried and dropped (user: drop if a lot of work). Added a third choice in World::unfork for a restore a crash stopped whose record says forking: spawn branch::resume_point_fork as upload::resumed does. Resumes then run: restores profile, 2000 seeds, 237 resumes, 19 Ok, 218 Err(Deleted). With resumes_refer off, no seed of 0..20000 found a violation (256 s, sim profile). A tuned profile (crash 6, delete 6, collect 12, open 2) gave 1 Ok resume in 2000 seeds. The violation needs: crash between the record and the reference, a resume while the point lives, then the point deleted and the base's cell collected before the clone's first open: rare. Next try, if picked up: a dedicated profile or a failpoint at point::recorded so the crash lands between record and reference, and resume always when the record says forking. Code reverted; test stays ignored.
<!-- SECTION:NOTES:END -->
