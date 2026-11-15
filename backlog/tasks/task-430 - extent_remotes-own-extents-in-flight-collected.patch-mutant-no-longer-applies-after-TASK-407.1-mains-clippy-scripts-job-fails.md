---
id: TASK-430
title: >-
  extent_remote's own-extents-in-flight-collected.patch mutant no longer applies
  after TASK-407.1: main's clippy scripts job fails
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 09:26'
updated_date: '2026-11-15 09:44'
labels:
  - bug
  - testing
dependencies: []
priority: high
ordinal: 821000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Main's CI 'test (clippy scripts)' fails since TASK-407.1 (c8344a37a, 3f500554b): scripts/test.sh's mutant check says crates/extent_remote/mutants/own-extents-in-flight-collected.patch doesn't apply (runs on 7edc54ef3, 1fd0855fb). Re-anchor the same mutation on the new collector code, and check its Killer: test still kills it.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Fixed in ed6b01be6: the mutation (an own extent no head names deleted though its segment may be acknowledged) re-anchored on 'Kind::Extent { .. } => goes(!named && !self.ahead(kind))'; mutants.sh --check clean (scripts part: all mutant sets 0 broken); mutants.sh own-extents-in-flight-collected: killed by sim::forks_racing_collection, seed 4.
<!-- SECTION:NOTES:END -->
