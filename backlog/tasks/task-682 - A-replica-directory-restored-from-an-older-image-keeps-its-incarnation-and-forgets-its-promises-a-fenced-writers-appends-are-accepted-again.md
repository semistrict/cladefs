---
id: TASK-682
title: >-
  A replica directory restored from an older image keeps its incarnation and
  forgets its promises: a fenced writer's appends are accepted again
status: To Do
assignee: []
created_date: '2026-11-20 00:59'
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
- [ ] #1 doc-21 and docs/operating-a-host.md state that a replica directory must never be restored or copied, and why
- [ ] #2 Decided and recorded in OPEN_QUESTIONS.md: whether the daemon detects a regressed directory (e.g. a generation counter checked against the head's configurations) or the rule stays operational
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
