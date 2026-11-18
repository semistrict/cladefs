---
id: TASK-646
title: >-
  The guide's build fails: anchors.ts names crates/cladefs/src/extents.rs, which
  TASK-508.3 removed
status: To Do
assignee: []
created_date: '2026-11-18 20:37'
labels:
  - docs
  - ci
dependencies: []
priority: medium
type: bug
ordinal: 1030000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
CI runs 44353952388 (8111fb784) and 44380042393 (ca72b5814) on main, 2026-11-18: the guide job's pnpm build fails in both with "ENOENT: no such file or directory, copyfile 'crates/cladefs/src/extents.rs' -> 'guide/src/generated/public/source/crates/cladefs/src/extents.rs'". guide/src/content/anchors.ts still points local_cell, es_write, es_commit, es_uploaded, es_map and maybe more at crates/cladefs/src/extents.rs, and the snippets they quote (pub struct ExtentStore, ...) were renamed by TASK-508.3 (8e9f18a42: local_cell and bucket_cell crates, OpenCell, CellClient, ...).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Every anchor in guide/src/content/anchors.ts names a file that exists and a snippet found in it, and the guide's text agrees with the new names (CONTEXT.md glossary).
- [ ] #2 main's guide CI job passes.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
