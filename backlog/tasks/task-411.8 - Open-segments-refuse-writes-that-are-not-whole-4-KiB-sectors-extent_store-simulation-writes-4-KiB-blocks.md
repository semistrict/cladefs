---
id: TASK-411.8
title: >-
  Open segments refuse writes that are not whole 4 KiB sectors; extent_store
  simulation writes 4 KiB blocks
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-14 19:07'
updated_date: '2026-11-14 19:07'
labels: []
dependencies: []
parent_task_id: TASK-411
type: task
ordinal: 805000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Encrypting open segments with AES-256-XTS per 4 KiB sector (TASK-411.5, doc-16) needs no read-modify-write only if every write reaching an open segment is whole sectors. A probe on 2026-11-14 (an assert in Segments::reserve and Segments::append) found every real path whole sectors on macOS: 574 tests of extent_store, sqlite_extents and fxdax passed; only the 62 of extent_store's simulation failed, as sim/world.rs writes 512-byte blocks. Nothing enforces it today.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A write or seal reaching an open segment whose length or offset is not a multiple of 4096 is refused with an error naming it, not written
- [ ] #2 extent_store's simulation writes 4 KiB blocks, and its tests pass
- [ ] #3 A test shows a misaligned write refused
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
