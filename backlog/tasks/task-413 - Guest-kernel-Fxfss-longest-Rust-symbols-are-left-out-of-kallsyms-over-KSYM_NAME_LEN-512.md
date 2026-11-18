---
id: TASK-413
title: >-
  Guest kernel: Fxfs's longest Rust symbols are left out of kallsyms (over
  KSYM_NAME_LEN 512)
status: To Do
assignee: []
created_date: '2026-11-14 20:03'
updated_date: '2026-11-17 01:10'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 805000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Building the arm64 guest kernel in Lima (scripts/lib/lima.sh build-guest-kernel.sh arm64, 2026-11-14) warns four times 'Symbol _RINv...cladefs_linux too long for kallsyms (513-537 >= 512)': v0-mangled names of fxfs's compaction drop glue (lsm_tree::compact_with_iterator ... CompactionYielder) and a bincode deserialize seed. kallsyms leaves them out, so an oops or a stack dump inside them names a neighbouring symbol instead. The build still succeeds. Not checked whether x86-64 warns too.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The guest kernel builds without 'too long for kallsyms' warnings, on arm64 and x86-64
- [ ] #2 A stack through fxfs's compaction drop glue names the right function
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
