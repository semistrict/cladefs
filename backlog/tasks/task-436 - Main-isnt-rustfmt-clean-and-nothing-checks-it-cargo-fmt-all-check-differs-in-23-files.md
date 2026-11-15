---
id: TASK-436
title: >-
  Main isn't rustfmt-clean and nothing checks it: cargo fmt --all --check
  differs in 23 files
status: To Do
assignee: []
created_date: '2026-11-15 11:24'
updated_date: '2026-11-15 14:30'
labels:
  - testing
  - ci
dependencies: []
priority: low
ordinal: 828000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
2026-11-15 (coordinator, from TASK-431's agent): cargo fmt --all --check on main 2fe3a766b+ shows diffs in 23 files (extent_frame segment.rs, extent_remote collector.rs, fetch.rs, layout.rs, branch.rs, head.rs, cache.rs, cladefs-gc.rs, extent_store sim/world.rs, wal.rs, ...), from several sessions' commits; neither scripts/test.sh nor ci.yml checks formatting. Fix: format the tree once at a quiet moment (many sessions edit these files; a tree-wide reformat conflicts with their branches), then make scripts/test.sh's clippy part (and so CI) run cargo fmt --all --check.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 cargo fmt --all --check passes on main
- [ ] #2 scripts/test.sh fails on an unformatted change (CI with it)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
