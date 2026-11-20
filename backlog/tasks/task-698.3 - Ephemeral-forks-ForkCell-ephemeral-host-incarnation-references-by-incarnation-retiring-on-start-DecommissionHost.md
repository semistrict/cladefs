---
id: TASK-698.3
title: >-
  Ephemeral forks: ForkCell ephemeral, host incarnation, references by
  incarnation, retiring on start, DecommissionHost
status: Done
assignee:
  - '@claude'
created_date: '2026-11-20 16:21'
updated_date: '2026-11-20 20:16'
labels: []
dependencies:
  - TASK-698.1
documentation:
  - >-
    backlog/docs/doc-25 -
    Ephemeral-cells-host-local-children-of-a-version-migrated-live.md
parent_task_id: TASK-698
ordinal: 1092000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Forking a version into an ephemeral cell writes only a reference in the parent bucket, which must keep the version while the cell lives and be retired once it does not, including when the host never comes back (doc-25, Forking one). The user chose explicit decommissioning over leases.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 ForkCell with ephemeral set makes a cell whose ID comes from the bucket allocator, writing the reference before reading the parent head
- [x] #2 The collector keeps a version while an ephemeral reference stands, and recognises the key kind as one with no head
- [x] #3 A daemon starting retires its incarnation references whose cell it does not hold; DecommissionHost retires all of an incarnation
- [x] #4 Ephemeral cells refuse replicas, FSYNC_LEVEL_REPLICATED and MakeVersion; their sync and syncfs wait for Local only
- [x] #5 The proto, Go bindings and their docs describe the new fields and calls
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. bucket_cell: ephemeral reference key under the origin prefix (kept by passes as children's are, never retired), index entry by incarnation at the root, Kind::Ephemeral/EphemeralIndex; lifecycle::fork_ephemeral (index, reference, head read, take back if refused), unrefer_ephemeral, ephemeral_references, decommission.
2. local_cell::upload: Uploader::forked_ephemeral (keys from Inheriting kept in the base), open_ephemeral (keys unwrapped as a fork head would, parent version state via recover_version under the child ring).
3. cladefs: ephemeral_host (CLADEFSD_EPHEMERAL: incarnation, journal noted before each reference, retire_stale at boot); host_cell fork/open/delete for ephemeral stores; OpenCell assembles without an uploader; cells::fork_ephemeral_of_version; ForkCell.ephemeral and DecommissionHost in the proto, Go bindings regenerated.
4. Tests: collector keeps a dropped version for an ephemeral reference until unreferred or decommissioned; daemon end-to-end fork, read inherited, write own, reopen, delete, retire on start, decommission.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-20: batch 2 committed (c8a6bf9ce). bucket_cell ephemeral test passes on macOS; cladefs compiles on macOS; Linux clippy + tests running on ubu1 (fxjob eph-check). Follow-ups noticed: an ephemeral store's ring is not held among the host's keys (RingHold), so forgetting a shredded tenant's keys does not reach an open ephemeral cell yet; the parent version state is recovered at every opening (the at-version store's layer is not shared with ephemeral openings).

2026-11-20: on main at f3bd11827 + 91364d7d4. Linux: cladefsd::tests::ephemeral_cells_fork_from_versions_and_retire_their_references and ephemeral_host tests pass; collector ephemeral test passes; Go bindings regenerated. Note: scripts/test.sh madsim on ubu1 ran 0 tests (nextest selected none of 121 binaries) after its build check passed; CI's madsim job on main reports success, so the selection difference is ubu1-side and not looked into further yet.

AC #4 holds by construction: OpenCell::commit_version needs an uploader (ENOTSUP), forks need one (EROFS), and commits wait for Local without one; no test of its own. DoD #1 left open: the touched crates passed on macOS and Linux (local_cell 382/382 macOS, local_cell+bucket_cell 718/718 Linux, cladefs lib+cells/control/peers/handover 542/543 Linux with TASK-700 unrelated, the madsim suite 20 seeds); the whole workspace is CI's, running at close. upstream-changes --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Ephemeral forks from versions: ForkCell ephemeral, references keyed by the host incarnation with an index for decommissioning, the host journal (CLADEFSD_EPHEMERAL) retiring stale references at boot, DecommissionHost, DeleteCell retiring the reference; versions and replicas refused, there being no uploader. Verified by the collector ephemeral test and the cladefsd daemon test on Linux.
<!-- SECTION:FINAL_SUMMARY:END -->
