---
id: TASK-702.1
title: >-
  Traces of cold boots of one version under two workloads, and the predictors
  compared on them offline
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-20 19:46'
updated_date: '2026-11-20 19:47'
labels:
  - sandbox
  - performance
  - cache
dependencies: []
parent_task_id: TASK-702
priority: high
ordinal: 1103000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
What a prediction of a version's reads should hold, and how it should decay, is to be chosen from data, not assumed. The daemon records no timed trace of a cell's reads today: the boot profile recorder (crates/cladefs/src/profile.rs) keeps units in first-read order with no times and no source, and the cache.page_in span has no offset and is sampled. The candidates to compare: today's read-ahead alone; the whole extent fetched on its first touch; the first boot's set kept forever (what JuiceFS does); recent boots' sets, decayed; and the oracle (each boot's own reads known in advance). Two workloads on one version, alternating, show how each adapts as reads drift, not only how it does on a repeat.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The daemon can record a timed trace of a cell's reads after it opens: per read the time since open, extent, offset, length, and where it was found (extent cache, part cache, peer, bucket)
- [ ] #2 Traces of at least six cold boots of one version on the test host, alternating two workloads (e.g. the python-sandbox case of TASK-665 and another command), are kept under docs/measurements
- [ ] #3 An offline evaluation, in the repo, replays the traces in order against each candidate predictor and reports per boot: recall of parts read in the first 10 s, bytes fetched that no read used, and round trips left on the critical path
- [ ] #4 The evaluation's results, and the predictor chosen with its parameters (evidence kept, decay, threshold), are recorded in this task and in docs/measurements
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
