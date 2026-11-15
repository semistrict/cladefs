---
id: TASK-467
title: >-
  written-back-twice.patch mutant no longer applies after TASK-433.5
  (extent_store dir.rs): main's clippy scripts job fails again
status: To Do
assignee: []
created_date: '2026-11-15 20:58'
labels:
  - bug
  - testing
dependencies: []
priority: high
ordinal: 874000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Main CI run 43758893191 (a252cb53f): test (clippy scripts) fails: crates/cladefs/mutants/written-back-twice.patch doesn't apply. TASK-461 re-anchored it on 1cff819be; 38c8470dd (TASK-433.5, CachedDir) changed crates/extent_store/src/dir.rs again. Re-anchor the same mutation; its Killer remote::write_budgets::a_hosts_store_writes_what_its_design_says must still kill it.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
