---
id: TASK-728
title: >-
  A host keeps a version's empty prediction for the whole interval, though the
  first boot's evidence lands seconds later
status: To Do
assignee: []
created_date: '2026-11-21 08:23'
labels:
  - sandbox
  - cache
dependencies: []
priority: low
type: bug
ordinal: 1125000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/cladefs/src/evidence.rs keeps a prediction read once per EVERY (10 min) per (version, tenant), an empty one (no recordings yet) too. The first host to boot a new version reads none, and its next forks within 10 min go unpredicted, though the first boot's recording is written 10 s in (ubu1-python-sandbox-2026-11-21T0806Z-ba0b7256: host-a's second boot read-unpredicted 2,024 units). A prediction from no recordings could be read again at the next open, as a failed read is, at the cost of a LIST a fork while none exists.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A host whose prediction of a version came from no recordings reads it again at its next fork's opening, and keeps one from recordings for the interval (test)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 scripts/upstream-changes.py --check passes
- [ ] #2 Deviations from upstream documented where they occur
<!-- DOD:END -->
