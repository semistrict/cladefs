---
id: TASK-422
title: >-
  The Firecracker series check builds old commits against today's cladefs
  crates: the extent commits before TASK-364 call fork_quiescent(point, branch),
  which takes one argument since
status: To Do
assignee: []
created_date: '2026-11-15 03:07'
updated_date: '2026-11-16 23:17'
labels:
  - bug
dependencies: []
priority: medium
ordinal: 810000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-417's series check on ubu1: scripts/firecracker-series-check.sh builds every commit of vendor/firecracker's series with --features cladefs against the cladefs crates of the checkout running it. TASK-364 changed RemoteExtentStore::fork_quiescent to take the point alone, and appended a fork commit using it; the series' commits before it (from "the extent-store device (virtio-extent)" on) still call it with a branch, so they no longer build, and the check fails on main as well. CI did not notice: the run of the series check for bd6ac11a1 (TASK-364) was cancelled, and later runs skip it as the submodule did not move. Structural question: any API change of cladefs's breaks every older commit of the series, so either each such change ports the series (a rewrite), or the check builds older commits against what they were written against.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The series check passes on main, every commit both ways
- [ ] #2 A change to cladefs's API the fork uses cannot land with the series check failing unseen (CI runs it, or the rule for porting the series is written down)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Seen again by TASK-501 (2026-11-16), whose move of vendor/firecracker to cladefs-v1.16.1-r9 made CI run the check (run 43974684702): the 8 commits from "the extent-store device" to TASK-364's fail, now on TASK-474's renames too (cladefs::RemoteExtentStore, Commit::Durable, a request tuple's unsized slice; ubu1 ~/src/fxdax-t501-series/1a81da3cf.log, removed after). The tip (TASK-417's device on virtio_extent) and TASK-501's commit after it pass both ways. TASK-502, filed for it before this task was found, is archived as its duplicate.
<!-- SECTION:NOTES:END -->
