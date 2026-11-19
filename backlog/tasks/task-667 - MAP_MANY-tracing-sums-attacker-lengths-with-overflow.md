---
id: TASK-667
title: MAP_MANY tracing sums attacker lengths with overflow
status: To Do
assignee: []
created_date: '2026-11-19 17:28'
labels:
  - security
dependencies: []
priority: low
type: bug
ordinal: 1050000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the 2026-11-19 kernel sandbox security review. **Where:** `crates/cladefs/src/cladefsd/server.rs:898-905`.

Tracing computes `mappings.iter().map(|mapping| mapping.len).sum()` before normal per-map range validation. Two entries of len u64::MAX overflow, panicking a checked daemon with tracing enabled or wrapping release trace accounting. **Structure:** tracing must use checked/saturating accounting and never change request validity or liveness.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Use checked accounting and preserve ordinary EINVAL behavior with tracing enabled
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
- [ ] #4 cargo nextest run --workspace passes on macOS and Linux (ubu1)
<!-- DOD:END -->
