---
id: TASK-702.1
title: >-
  Traces of cold boots of one version under two workloads, and the predictors
  compared on them offline
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-20 19:46'
updated_date: '2026-11-20 20:38'
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
- [ ] #1 An offline evaluation, in the repo, replays the traces in order against each candidate predictor and reports per boot: recall of parts read in the first 10 s, bytes fetched that no read used, and round trips left on the critical path
- [ ] #2 The evaluation's results, and the predictor chosen with its parameters (evidence kept, decay, threshold), are recorded in this task and in docs/measurements
- [x] #3 With CLADEFSD_READ_TRACES naming a directory, the daemon writes a trace of each VM's reads of its cell from its open: per DAX fault and READ, the time since open, the extent and its length, the offset and length read, and how long the guest waited (test)
- [ ] #4 Traces of at least six boots of one version on the test host, alternating two workloads, cold and warm hosts both (faults record a VM's first touches whether or not the extent cache holds the pages), are kept under docs/measurements
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. crates/cladefs/src/reads.rs: a per-VM read trace (time since open, kind, extent, extent length, offset, length, wait), bounded, written once as text when its connection goes; CLADEFSD_READ_TRACES names the directory, checked at start. The connection (cladefsd/server.rs) records each READ and each DAX fault (DaxWindow tells the extent at a window offset). Not page-ins: they miss a warm host's reads, and include read-ahead. Tests: format; a daemon test whose READs and faults land in the trace.
2. fxfs-guest-python-sandbox: a second workload alternating with the notebook, CLADEFSD_READ_TRACES per host into the results, rounds enough for six boots or more.
3. scripts/read_prediction.py (tests in scripts/tests): replays a version's traces in open order against the candidates (read-ahead alone, whole extent on first touch, first set forever, recent sets decayed, oracle) in 1 MiB units of each extent; per boot recall in the first 10 s, bytes fetched unused, and waits left, with a prefetch model of explicit concurrency and latency.
4. Run on ubu1 (scripts/python-sandbox-host.sh, fxjob --bench); evaluate; record the results and the chosen model here and in docs/measurements.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Steps 1-3 landed (7c006e3b9): CLADEFSD_READ_TRACES (crates/cladefs/src/cladefsd/reads.rs; READs and DAX faults per VM connection, 120 s from open), python-sandbox workloads notebook/query with traces kept, scripts/read_prediction.py with tests. query.py verified on the real 2025 data: {"trips": 591375, "trips_in_2025": 591354}. ubu1: cladefs Linux check, 39 tests (a_vms_reads_are_traced, reads, dax) and guest harness checks/tests pass; macOS shuttle, loom, madsim checks pass. Trace run started (scripts/python-sandbox-host.sh, 4 rounds, notebook,query).

Run 1 (docs/measurements/ubu1-python-sandbox-2026-11-20T2015Z-89f37d12): 12 sandboxes, all answered. Warm boots recorded 0-11 faults: a MAP installs pages already in memory with no fault reaching the daemon, so traces need MAPs (7714fae6c), and so will 702.2's evidence. Cold traces: notebook boots read the same 1,999 extents (Jaccard 1.0), query's 816 all but 2 inside them (~19 MiB more for the notebook than query's 186 MiB); Fxfs extents are 1 MiB at most, so a unit is an extent; 63-77% of extents read have their predecessor read. Preliminary: history sets recall 1.00, ~66 MiB unused a boot; next (online) recall 0.74-0.95, 317-1,726 MiB unused, half the waiting left.
<!-- SECTION:NOTES:END -->
