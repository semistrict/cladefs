---
id: TASK-698.1
title: >-
  TLA+ spec of ephemeral forks, reference retiring, decommissioning and live
  migration hand-over
status: Done
assignee:
  - '@claude'
created_date: '2026-11-20 16:21'
updated_date: '2026-11-20 19:56'
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
- [x] #1 specs/ephemeral checks NothingNeededGone, OneWriter and HandOverLosesNothing, with hosts dying at every step of a fork and a migration
- [x] #2 Each rule (forksReferFirst, targetRefersFirst, sourceGivesUpFirst, sourceRetiresLast, retireOnlyUnheld) has a mutation that TLC shows breaking an invariant
- [x] #3 Every model runs under ten minutes on the Mac, and liveness models show a fork, a retire and a migration being reached
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-20: 15 models on the Mac in 3 min 8 s: Ephemeral (1 cell, 2 hosts, 3 migrations, a crash, a loss, 2 passes) 2.36M states 20 s; EphemeralThree 19.3M states 153 s; the 8 rule mutations each violate their invariant (the AC named 5; refuseFinal, openAtNamed, remembersAccepted added); 5 Reaches models: hand-over, back and forth, on three hosts, collection, a retirement on start. DoD: the task changed no Rust; upstream-changes not touched.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
specs/ephemeral models ephemeral forks referring by incarnation, retirement on start, decommissioning and live migration hand-over (doc-25). Invariants NothingNeededGone, OneWriter, HandOverLosesNothing hold over Ephemeral and EphemeralThree; each of the 8 rules has a mutation TLC shows breaking one; Reaches models show hand-overs, collection and retirement reached. Verified with scripts/check-tla.sh on the Mac, every model under 3 minutes.
<!-- SECTION:FINAL_SUMMARY:END -->
