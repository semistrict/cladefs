---
id: TASK-453
title: >-
  An open fork holds 220-470 B of heap per extent of its base (its store's own
  map, and its writer's when forked from a live store): share what forks inherit
status: To Do
assignee: []
created_date: '2026-11-15 13:15'
updated_date: '2026-11-15 14:48'
labels:
  - perf
  - review-2026-11-15
dependencies: []
references:
  - TASK-443.2
  - TASK-242
priority: high
ordinal: 849000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Measured by TASK-443.2 (docs/measurements/macos-fork-extent-memory-2026-11-15/note.md; cladefs remote::tests::bench_fork_extent_memory, heap counted exactly). Every open, idle fork holds a private HashMap of every extent its branch inherited, each entry with an owned key String (extent_store Recovering::open): 238 B an extent at 32,768 extents (130 B table, 107 B key), 221 B at 300,000. A fork of a live store (a checkpoint of its own per fork) also holds its writer's map unshared, 230 B an extent more: 468 B (451 B at 300,000); forks of a point share that one. Measured: 100 forks of a 32,768-extent base hold 0.78 GB (point) or 1.41 GB (live); of a 300,000-extent base 6.6 GB or 12.4 GB. Extrapolated to 400 forks: 3.1 / 6.1 GB at 32,768, 26 / 54 GB at 300,000. None of it is counted: the memory governor counts extent-cache pages and held parts only, and the anon budget (extentd/process.rs AnonBudget) has no part for it while adding 288 MiB per open store for framing, so it would not flag it either; the daemon is OOM-killed with every VM. extent_remote's Decoded cache also keeps 16 decoded states whatever their size (16 x 69 MB at 300,000 extents). TASK-242 names the same copy for open time (measured here: 6-9 ms a fork at 32,768 extents, 40-80 ms at 300,000, sim profile).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 bench_fork_extent_memory: an idle fork of a base of 32,768 and of 300,000 extents holds heap that does not grow with the base's extents (per fork, of a point and of a live store), the numbers recorded beside TASK-443.2's
- [ ] #2 What the daemon still holds per extent, once per base, is counted by something the governor or the anon budget sees, or bounded
- [ ] #3 extentd's serves_hundreds_of_forks_of_a_base runs at 32,768 extents on Linux and bounds resident memory per fork
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
