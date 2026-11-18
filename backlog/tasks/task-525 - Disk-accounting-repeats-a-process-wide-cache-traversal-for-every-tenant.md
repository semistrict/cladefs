---
id: TASK-525
title: Disk accounting repeats a process-wide cache traversal for every tenant
status: To Do
assignee: []
created_date: '2026-11-18 12:24'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 915000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of revision 845a6c123, finding 10.

Every ten seconds, `crates/cladefs/src/cladefsd/disk.rs:275` loops over live disks. Each recount calls process-wide write-back at line 209, which visits every registered file at `crates/local_cell/src/pages.rs:296`. T disks and F registered files therefore incur at least T × F file visits per pass, even on an idle host. Scope write-back to each disk or share one correctly ordered flush across the pass. Verify counter snapshots still cover writes admitted during recount.

Evidence is from source inspection. Runtime reproduction is outstanding.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A full recount visits each relevant cache file only a bounded number of times independent of tenant count. Writes admitted during recount remain accounted for.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
