---
id: TASK-702.2
title: >-
  Read evidence: each recording of a version's reads is an object written once
  beside the version
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-20 19:46'
updated_date: '2026-11-21 00:46'
labels:
  - sandbox
  - cache
dependencies:
  - TASK-702.1
parent_task_id: TASK-702
priority: high
ordinal: 1104000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
A prediction of a version's reads must reach every host and outlive daemon restarts (TASK-38's profiles live in a per-pid temp directory, TASK-670), but forks that record are other cells, often on other hosts, and only a cell's writer changes it. Evidence objects created once, at keys of their own, need no writer and no in-place merge, as segments need none (TASK-407). Recency and expiry make the decay; nothing is rewritten. A shared version's evidence grown from other tenants' forks would tell every tenant what one tenant read.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A fork of a version, recording, writes its evidence as a new object beside the version by a conditional create, in a format of its own (refused if unknown), encrypted under the version's cell's keys
- [ ] #2 Only forks of the version's own tenant write evidence; other tenants' forks of a shared version read it and write none (test)
- [ ] #3 The collector deletes a version's evidence with the version, and evidence past the kept count or age the chosen model sets (test, and the collector spec if it models version objects)
- [ ] #4 A host reads a version's evidence once per version, not on every fork it opens
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From TASK-702.1 (2026-11-20): a recording must take the VM's MAPs (and READs), not its faults alone: a warm host's VM faults on none of what its MAPs install (crates/cladefs/src/cladefsd/reads.rs records all three). Units are MiBs of extents (an Fxfs extent is a MiB at most). Keep the last K = 8 recordings; older ones are what the collector removes beyond the count.
<!-- SECTION:NOTES:END -->
