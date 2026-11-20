---
id: TASK-682
title: >-
  A replica directory restored from an older image keeps its incarnation and
  forgets its promises: a fenced writer's appends are accepted again
status: Done
assignee:
  - '@claude'
created_date: '2026-11-20 00:59'
updated_date: '2026-11-20 04:25'
labels:
  - durability
  - peers
dependencies: []
references:
  - crates/local_cell/src/replica.rs
priority: low
type: task
ordinal: 1063000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found 2026-11-19 comparing with FoundationDB, which assumes disks never regress (simulation flags it as DBRecoveryDurabilityError) and has operators fence snapshotted or suspended hosts externally. A cladefs host whose disk is restored from a snapshot or backup, or a VM image cloned with its replica directory, comes back with the same incarnation and an older promise (or none) per cell: a zombie writer's appends of an epoch a take fenced are accepted again, which the incarnation rule (BookKeeper's cookies, the spec's NoIncarnation) exists to prevent for lost disks only. doc-21 does not state the assumption.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 doc-21 and docs/operating-a-host.md state that a replica directory must never be restored or copied, and why
- [x] #2 Decided and recorded in OPEN_QUESTIONS.md: whether the daemon detects a regressed directory (e.g. a generation counter checked against the head's configurations) or the rule stays operational
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-19: the user chose a documented rule only, no detection (explained: a per-address generation object in the bucket, bumped at each start, was offered).

AC evidence: doc-21 (What a member holds) and docs/operating-a-host.md (Replicated cells' replica directories) state the rule and why; OPEN_QUESTIONS.md records the user's choice (documented rule, not detected).
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Decided with the user: a replica directory restored or copied is not detected; doc-21 and docs/operating-a-host.md say never to restore or copy one, and why (it keeps its incarnation but forgets its promises, so a fenced writer's appends are taken again). Verified on the branch, landed as c9c8234ac..522dcce5b: macOS workspace 2812/2812 and local_cell/bucket_cell/cladefs 1125/1125 (every simulation); ubu1 under fxjob clippy + workspace 3124/3124; TLC on the Mac: every non-deep replication model holds, all 15 mutations violate, deep ReplicationDeferReplaces holds (21.4M states); scripts/upstream-changes.py --check passes.
<!-- SECTION:FINAL_SUMMARY:END -->
