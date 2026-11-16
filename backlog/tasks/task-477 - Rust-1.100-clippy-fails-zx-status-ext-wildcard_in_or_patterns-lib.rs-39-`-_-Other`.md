---
id: TASK-477
title: >-
  Rust 1.100: clippy's default lints flag 14 places in zx-status-ext and cladefs
  (CI unaffected)
status: To Do
assignee: []
created_date: '2026-11-16 03:34'
updated_date: '2026-11-16 04:38'
labels:
  - ci
  - bug
dependencies: []
priority: low
ordinal: 882000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found on ubu1 (rustc 1.100.0, 2026-11-09) running full clippy (-D warnings) for TASK-475. Correction (same day): CI and scripts/test.sh run clippy with -A clippy::all (only disallowed_methods and compiler warnings denied), so none of these fails CI. They are what the default lints say under 1.100, for whoever runs full clippy: wildcard_in_or_patterns in crates/zx-status-ext/src/lib.rs:39-55 (copied from Fuchsia: a fix is a documented deviation); in cladefs, redundant_slicing codec.rs:391,392,394,397,399,434,440,481; type_complexity metrics.rs:431; collapsible_if stores.rs:275; borrow_deref_ref wire.rs:1056; option_map_unit_fn store_table.rs retire().
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 cargo clippy --workspace --all-targets -- -D warnings passes on Rust 1.100
- [ ] #2 The deviation from upstream is documented in the file
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Also on 1.100, in cladefs (-p cladefs --no-deps), code TASK-475 did not touch: redundant_slicing codec.rs:391,392,394,397,399,434,440,481; type_complexity metrics.rs:431; collapsible_if stores.rs:275; deref on an immutable reference wire.rs:1056; option_map_unit_fn store_table.rs (retire: cut.take().map(|cut| cut()), cut.map(|cut| cut())). ubu1 run of TASK-475 lists them all.
<!-- SECTION:NOTES:END -->
