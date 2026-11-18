---
id: TASK-641
title: >-
  Main's scripts part fails: fxfs-posix's
  a-volume-frees-what-its-snapshot-shares mutant and two guest cocci mutants no
  longer apply
status: To Do
assignee: []
created_date: '2026-11-18 18:09'
labels:
  - testing
dependencies: []
priority: high
type: bug
ordinal: 951000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
scripts/test.sh's scripts part (CI's clippy scripts job) fails at origin/main 03b655f63, found merging TASK-539: crates/fxfs-posix/mutants.sh --check reports a-volume-frees-what-its-snapshot-shares.patch broken (40 checked, 1 broken); scripts/tests/test_cocci_guest.py's test_each_rule_finds_the_mutants_it_names_and_each_mutant_has_a_rule_or_a_why fails: guarded-fields.cocci's task-44.19-map-to-unlocked and lock-not-held.cocci's task-44.9-regions-find-unlocked mutants don't apply to guest/linux/fs/cladefs/dax.c (8111fb784 changed it). Each is to be retargeted and still found.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 crates/fxfs-posix/mutants.sh --check reports none broken
- [ ] #2 python3 -m unittest discover -s scripts/tests passes
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
