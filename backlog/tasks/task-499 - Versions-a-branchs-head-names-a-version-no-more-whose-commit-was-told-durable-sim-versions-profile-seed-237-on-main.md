---
id: TASK-499
title: >-
  Versions: a branch's head names a version no more whose commit was told
  durable (sim versions profile, seed 237 on main)
status: Done
assignee: []
created_date: '2026-11-16 16:28'
updated_date: '2026-11-16 16:46'
labels:
  - versions
  - sim
  - bug
dependencies: []
priority: high
ordinal: 888000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The simulation's versions profile (TASK-476), run past its 200 seeds (SIM_PROFILE=versions in sim::one, seeds 0..3000), fails on main 3934f820b at seed 237: 'branch 2 names version f no more, its commit 16 told durable: its head Head { epoch: 6, holder: 128, ..., versions: [Version { id: 59, at: 2, .. }] }'. Version f was neither dropped nor being dropped in the world's model. On t3/explore-tenant-encryption-keys, whose requests differ (encryption), the default 200 seeds hit it, seed 87: a head write conditional on the version that named the version (83 B, request 71) wrote a head of 65 B naming none (request 80, applied, answered Unavailable), then took the head at epoch 2 (holder 17). Some head writer builds its next head without the versions the head it swaps from names: a takeover after an ambiguous write, or a swap from a stale held head. sim::one lacked the versions profile; added on the branch.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The writer that drops a version not dropped is found, and a direct test fails without the fix
- [x] #2 The versions profile passes seeds 0..3000 on main
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Not a writer's bug: the simulation's model. A drop of a version whose head swap took effect, its answer lost (Unavailable after), the head then taken over before the swap's retry found it its own, fails Fenced as one that didn't land; the model took every Err as dropped not, and then found the version gone. Found by logging each head write with its backtrace (seed 87 on the branch: the drop's swap from the version's own head wrote none). sim::World's VersionDropped now reads the head on an Err and counts the version dropped iff the head names it no more; Writer::drop_versions says it may drop on failing. sim::one gains the versions profile. Seed 87 (branch) and 237 (main) pass, and seeds 0..3000 of the versions profile pass on the branch; versions_cut_dropped_and_restored's 200 seeds include 87.
<!-- SECTION:FINAL_SUMMARY:END -->
