---
id: TASK-633.10
title: >-
  Deterministic simulation of cells across buckets: regions, region outages,
  routing, forks, mirrors and region-partitioned peers
status: In Progress
assignee: []
created_date: '2026-11-18 22:54'
updated_date: '2026-11-19 03:01'
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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-18 night (unattended), first slices on branch (head 3e3a32e9b, not yet on main):
- f58fce990 SimBucket holds region buckets: one queue for all (a seed orders them all), SimBucket::at(home) views route by layout::routed_to (shared with ObjectBucket's router), SimBucket::down(region). SimBucket::new() routes nothing: old seeds/journals unchanged.
- 16997b078 World Profile::regions (0 = as before, no RNG draws): host H in bucket 1 + H % regions; root and restored children take IDs of their maker's bucket; a region's bucket flips down/up at times (REGION_FLIPS 0.02/step, all up when quiescing); collectors and final sweeps per region; Direct (checks) lists all regions for no-cell prefixes. Profile cells_across_regions (deleted_bases + 3 regions): 200 seeds hold, seed 0 sends 421/1263 requests to non-home regions; in seeds_replay. sim 42/42.
- First runs found two World gaps (sweeping only the home region; uncollected listing only the home region), fixed; no code bug found yet.
Left for the ACs: registry/additions and identity in the World (#1); the root bucket unreachable and cross-region latency as longer delays (#2); peers and the part cache (#3); specs/buckets invariants + per-rule seed searches + mutants (#4); mirror/outage profiles in the nightly hunt (#5).
<!-- SECTION:NOTES:END -->
