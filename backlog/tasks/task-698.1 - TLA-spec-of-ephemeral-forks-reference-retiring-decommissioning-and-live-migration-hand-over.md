---
id: TASK-698.1
title: >-
  TLA+ spec of ephemeral forks, reference retiring, decommissioning and live
  migration hand-over
status: To Do
assignee: []
created_date: '2026-11-20 16:21'
labels: []
dependencies: []
documentation:
  - >-
    backlog/docs/doc-25 -
    Ephemeral-cells-host-local-children-of-a-version-migrated-live.md
parent_task_id: TASK-698
ordinal: 1090000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Ephemeral cells add three protocols that touch other hosts and the bucket: references held by host incarnation, retiring them on restart or decommission, and handing a cell from S to T. The project checks each such protocol in TLA+ before coding (doc-23, doc-21). The rules are listed in doc-25, The spec.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 specs/ephemeral checks NothingNeededGone, OneWriter and HandOverLosesNothing, with hosts dying at every step of a fork and a migration
- [ ] #2 Each rule (forksReferFirst, targetRefersFirst, sourceGivesUpFirst, sourceRetiresLast, retireOnlyUnheld) has a mutation that TLC shows breaking an invariant
- [ ] #3 Every model runs under ten minutes on the Mac, and liveness models show a fork, a retire and a migration being reached
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
