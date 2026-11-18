---
id: TASK-642
title: fxfs-posix mutants.sh exits 0 when a mutant's test run couldn't start
status: To Do
assignee: []
created_date: '2026-11-18 20:08'
labels:
  - testing
dependencies: []
priority: low
type: bug
ordinal: 1028000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/fxfs-posix/mutants.sh reports each mutant's fate but exits non-zero only for patches that don't apply: a run whose cargo nextest couldn't start (exit 127, cargo not on PATH in a non-login ssh on ubu1, 2026-11-18, found by TASK-641) or didn't build prints 'exit N: ...' and the script exits 0, as if every mutant had been tried. crates/cladefs/mutants.sh counts such a run BROKEN and fails.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A mutant whose test run exits other than 0 or 100 makes crates/fxfs-posix/mutants.sh exit non-zero
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
