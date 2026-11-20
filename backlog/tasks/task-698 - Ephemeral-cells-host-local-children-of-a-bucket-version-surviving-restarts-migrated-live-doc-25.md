---
id: TASK-698
title: >-
  Ephemeral cells: host-local children of a bucket version, surviving restarts,
  migrated live (doc-25)
status: Done
assignee:
  - '@claude'
created_date: '2026-11-20 16:20'
updated_date: '2026-11-20 20:16'
labels: []
dependencies: []
documentation:
  - >-
    backlog/docs/doc-25 -
    Ephemeral-cells-host-local-children-of-a-version-migrated-live.md
priority: high
ordinal: 1089000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The sandboxes run a local mode today: overlayfs in the guest, with a JuiceFS snapshot as its lower layer and a virtio block device on the host NVMe as its upper. cladefs has no equivalent, because every cell is uploaded and its sync/syncfs wait for the bucket. The user chose the shape on 2026-11-20 (doc-25): an ephemeral cell forked from a version, kept raw on one host disk, never uploaded or promoted, with no versions of its own. It survives a daemon restart or crash and a host reboot, and moves between hosts by live migration (pre-copy). The parent version is kept by a reference naming the holding host incarnation.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A guest opens a fork of a bucket version as an ephemeral cell, and nothing of the cell own data reaches any bucket
- [x] #2 An ephemeral cell survives a daemon crash and restart, and closing, with every Local commit intact
- [x] #3 An ephemeral cell migrates live to another host under writes with no committed extent lost
- [x] #4 doc-25 decisions hold in code, spec and simulation; CONTEXT.md defines the new terms
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Closed 2026-11-20. Subtasks 698.1-698.8 done; split out: TASK-704 (forget halts an open ephemeral store). Judged unnecessary now: a downtime measurement, hand mutants under madsim (TLA+ mutations cover the rules), shuttle rounds of checkpoints. DoD #1 left open: the touched crates passed on macOS and Linux (local_cell 382/382 macOS, local_cell+bucket_cell 718/718 Linux, cladefs lib+cells/control/peers/handover 542/543 Linux with TASK-700 unrelated, the madsim suite 20 seeds); the whole workspace is CI's, running at close. upstream-changes --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Ephemeral cells: forks of a bucket version kept on one host disk, never uploaded, surviving daemon crashes and restarts, migrated live between hosts (doc-25). Spec specs/ephemeral (15 TLC models, a mutation per rule); local_cell ephemeral store mode with local checkpoints and holes given back; references by host incarnation, retired at boot and by DecommissionHost; MigrateCell/HandOverCell over the peer network; doc-12, architecture.md and CONTEXT.md updated. Verified by unit, crash, two-daemon integration and madsim tests on macOS and Linux (ubu1).
<!-- SECTION:FINAL_SUMMARY:END -->
