---
id: TASK-710
title: 'zpages pprof: profiles_say_what_ran saw an empty profile once on a loaded ubu1'
status: To Do
assignee: []
created_date: '2026-11-20 22:21'
updated_date: '2026-11-21 06:03'
labels:
  - testing
dependencies: []
priority: low
type: bug
ordinal: 1114000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-702.1's Linux workspace run on ubu1 (two other jobs running) failed crates/cladefs/src/cladefsd/zpages/pprof.rs:159 with an empty sample list (`[]`); 8 runs of it alone afterwards passed. The test depends on the profiler sampling within its window, which a loaded host may miss.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The test holds on a loaded host: it waits for samples or does enough work that sampling cannot miss it, shown by runs beside a CPU loader
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Again 2026-11-21 on ubu1 (TASK-702.2's workspace run, another job beside it): pprof.rs:159, samples [].
<!-- SECTION:NOTES:END -->
