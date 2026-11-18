---
id: TASK-633.10
title: >-
  Deterministic simulation of cells across buckets: regions, region outages,
  routing, forks, mirrors and region-partitioned peers
status: To Do
assignee: []
created_date: '2026-11-18 22:54'
labels:
  - bucket
  - multi-region
  - sim
dependencies: []
references:
  - TASK-633.2
  - TASK-28.1
documentation:
  - >-
    backlog/docs/doc-23 -
    Cells-across-buckets-a-bucket-per-cloud-region-forks-that-span-them.md
parent_task_id: TASK-633
priority: high
type: feature
ordinal: 1033000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The user asked (2026-11-18) for FoundationDB-style simulation testing of the multi-region work. local_cell's deterministic simulation (crates/local_cell/src/sim, TASK-28.1, TASK-88) already runs the real stores, uploaders, collectors, forks and takeovers on hosts that crash, over one SimBucket that delays, reorders and fails requests, seeded and replayable (a seed must replay its requests exactly), with invariants after every step, a seed search per rule, SIM_SOAK and SIM_TRACE, and hand mutants. It has one bucket and no peers or part cache. Extend World rather than build a new framework, in step with the code: routing with TASK-633.4, forks across buckets with TASK-633.7, mirrors with TASK-633.8. specs/buckets (TASK-633.2) checks the protocol; this checks the code implements it.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 World holds a bucket per region, hosts each with a home bucket, the root bucket's registry and additions, and requests routed by the cell's bucket number; seeds still replay exactly
- [ ] #2 Region faults: a whole bucket down for a stretch, the root bucket unreachable, cross-region latency as longer delays than in-region ones
- [ ] #3 Peers and the part cache in the simulation, partitioned by region: an invariant that no peer request ever crosses a region, and that a remote part is fetched across regions at most once per region while cached
- [ ] #4 specs/buckets' invariants checked on the real code after every step (every live child reads its version whole wherever it lives; a marker never names an incomplete copy; a done mirror leaves no child depending on its base's region), each cross-bucket rule with a seed search that finds its violation when it is off, and hand mutants in crates/bucket_cell/mutants.sh
- [ ] #5 Profiles for forks landing at every point of a mirror, racing collectors, region outages and crashes, run by scripts/test.sh's sim part and the nightly hunt
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
