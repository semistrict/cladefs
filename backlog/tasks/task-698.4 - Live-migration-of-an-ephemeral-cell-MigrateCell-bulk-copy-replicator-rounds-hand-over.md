---
id: TASK-698.4
title: >-
  Live migration of an ephemeral cell: MigrateCell, bulk copy, replicator
  rounds, hand-over
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-20 16:21'
updated_date: '2026-11-20 19:49'
labels: []
dependencies:
  - TASK-698.1
  - TASK-698.2
  - TASK-698.3
documentation:
  - >-
    backlog/docs/doc-25 -
    Ephemeral-cells-host-local-children-of-a-version-migrated-live.md
parent_task_id: TASK-698
ordinal: 1093000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Sandboxes on ephemeral cells must move between hosts while running. The user chose pre-copy; guest RAM is the VM manager job. Sealed extents never change, so rounds converge, and doc-21 replicator already streams a cell log to a peer (doc-25, Live migration).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 MigrateCell on S writes T reference first, sends closed raw segments sparse, then streams rounds until the VM manager asks for the hand-over
- [ ] #2 At hand-over S commits, sends the rest and a local checkpoint, gives the cell up, and only then names its last commit; T opens only for that commit
- [ ] #3 Hand-overs failing before and after S gives up are tested: S stays writer, or T becomes it, never both
- [ ] #4 A guest case migrates a running VM disk between two daemons under writes; downtime and bytes sent are recorded on the test host
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Peer RPCs MigratePrepare/File/HandOver/Abort; control RPCs MigrateCell/HandOverCell; Go bindings.
2. Target: journal entry with the migration, reference first, staging beside the path, accepted/ for good, refusal drops staging and reference, restart gives a migration up.
3. Source: rounds send kept segments; hand-over writes the marker, sends directories, the rest and new holes, names files with lengths and CRCs, then removes its store and retires its reference, or removes its marker.
4. Opening refuses a marked store; ephemeral rings held among host keys while open.
5. Tests: two daemons, live migration with writes during rounds; target unreachable then restarted and refusing.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-20 Linux (ubu1): ephemeral_cells_migrate_live_between_hosts (340 MiB, writes during rounds) and hand_overs_refused_leave_the_source_serving pass; cladefs --lib + cells/control/peers/handover: 542/543, the one failure TASK-700, a control shred test passing alone, filed. Design differs from doc-25's first draft: files copied, not the log streamed (doc-25 updated). Not covered yet: a hand-over refused for a missing file, a source restart while given up, three hosts (remembersAccepted in practice); downtime not measured.

2026-11-20: Linux: hand_overs_naming_files_the_target_lacks_are_refused (openAtNamed, refuseFinal) and the source restarted while given up (in hand_overs_refused_leave_the_source_serving) pass with the live migration test. Left: three hosts (remembersAccepted in practice), downtime measured on the test host.
<!-- SECTION:NOTES:END -->
