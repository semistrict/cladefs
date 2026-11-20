---
id: TASK-693
title: >-
  Mutant patches bucket-failures-dropped, tenants-disks-unlimited,
  writes-retried-at-once no longer apply on main: test (clippy scripts) red on
  main's CI
status: Done
assignee: []
created_date: '2026-11-20 13:58'
updated_date: '2026-11-20 14:04'
labels: []
dependencies: []
priority: high
ordinal: 1083000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
main CI run on 305aa2033: scripts part's mutant check reports 'crates/cladefs/mutants/{bucket-failures-dropped,tenants-disks-unlimited,writes-retried-at-once}.patch doesn't apply', 22 checked, 3 broken.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Remade bucket-failures-dropped, tenants-disks-unlimited, writes-retried-at-once, and bucket_cell's versions-named-past-their-cuts (also stale, hidden behind the first failure). Every mutants.sh --check: 0 broken.
<!-- SECTION:NOTES:END -->
