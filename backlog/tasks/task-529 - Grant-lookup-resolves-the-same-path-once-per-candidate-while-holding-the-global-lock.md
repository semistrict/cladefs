---
id: TASK-529
title: >-
  Grant lookup resolves the same path once per candidate while holding the
  global lock
status: To Do
assignee: []
created_date: '2026-11-18 12:25'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 919000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of revision 845a6c123, finding 14.

In `crates/cladefs/src/cladefsd/grants.rs:90` and line 98, each directory comparison invokes `in_dir`, which calls `store_key` at `crates/cladefs/src/cladefsd/server.rs:952`. An unmatched path incurs filesystem lookups for every grant, blocking other grant operations meanwhile. Resolve each input once, then compare it against canonical tenant directories.

Evidence is from source inspection. Runtime reproduction is outstanding.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Filesystem path resolution per lookup stays bounded independent of grant count. Tenant membership results match across existing and newly created store paths.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
