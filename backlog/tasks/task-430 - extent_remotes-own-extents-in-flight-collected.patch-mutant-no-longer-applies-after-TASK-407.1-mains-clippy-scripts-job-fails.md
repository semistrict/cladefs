---
id: TASK-430
title: >-
  extent_remote's own-extents-in-flight-collected.patch mutant no longer applies
  after TASK-407.1: main's clippy scripts job fails
status: Done
assignee:
  - '@claude'
created_date: '2026-11-15 09:26'
updated_date: '2026-11-15 10:00'
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
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Fixed in ed6b01be6: the mutation (an own extent no head names deleted though its segment may be acknowledged) re-anchored on 'Kind::Extent { .. } => goes(!named && !self.ahead(kind))'; mutants.sh --check clean (scripts part: all mutant sets 0 broken); mutants.sh own-extents-in-flight-collected: killed by sim::forks_racing_collection, seed 4.

2026-11-15, verified independently on the Mac (the same hunk as ed6b01be6's, made by a second session): mutants.sh --check 2 checked, 0 broken; mutants.sh own-extents-in-flight-collected killed by sim::forks_racing_collection, seed 4 (own extents of branch 2 gone; branches 2, 48, 58 lost acknowledged extents), 64 s; the killer clean (--cargo-profile sim): PASS, 8.3 s. scripts' part otherwise: unittest OK, check-exclusions, license-headers, every mutants --check (40, 4, 22, 2, 58) pass.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Fixed (see notes); verified: ubu1 480962251: workspace 2786/2787 (TASK-315's ZFS page-cache test alone); macOS gate, go and scripts parts passing.
<!-- SECTION:FINAL_SUMMARY:END -->
