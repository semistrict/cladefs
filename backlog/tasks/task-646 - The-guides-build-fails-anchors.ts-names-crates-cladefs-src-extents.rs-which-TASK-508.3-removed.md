---
id: TASK-646
title: >-
  The guide's build fails: anchors.ts names crates/cladefs/src/extents.rs, which
  TASK-508.3 removed
status: Done
assignee:
  - '@claude'
created_date: '2026-11-18 20:37'
updated_date: '2026-11-20 00:01'
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
- [x] #1 Every anchor in guide/src/content/anchors.ts names a file that exists and a snippet found in it, and the guide's text agrees with the new names (CONTEXT.md glossary).
- [x] #2 main's guide CI job passes.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Anchors moved to TASK-508.3's names (open_cell.rs OpenCell, CellClient, LocalCell, upload.rs forked, layout.rs Origin; flush_behind's Arc<OpenCell>); keys renamed to match (open_cell, cell_client, local_cell, local_commit, fork_head); layers and invariants say cell where they said branch (glossary: Cell); pagePilot's extents carry their cell; guide tests at open_cell.rs/OpenCell. guide: node tools/check.ts (63 anchors, 12 invariants, 11 traces match), pnpm build, pnpm test 42/42.

CI run 44629552079 (e7fbf3abb): its job passes. Workspace: macOS 2754/2754; Linux (ubu1, fxjob, with TASK-644/675's changes too) 3096/3097, the one failing mappings_asked_for_past_the_credit_let_their_vm_process_go, TASK-546's, failing on main before. DoD #1 left unchecked for that. scripts/upstream-changes.py --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
The guide's anchors and layers named crates/cladefs/src/extents.rs and types TASK-508.3 renamed; they now point at open_cell.rs (OpenCell), client.rs (CellClient), local_cell's cell.rs (LocalCell) and upload.rs (forked), bucket_cell's layout.rs (Origin), the keys renamed to match, and the guide's text says cell where it said branch. Verified: tools/check.ts (63 anchors, 12 invariants, 11 traces), pnpm build, pnpm test 42/42, and main's guide CI job (run 44629552079).
<!-- SECTION:FINAL_SUMMARY:END -->
