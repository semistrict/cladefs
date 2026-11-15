---
id: TASK-434
title: >-
  A segment sampled for framing in place, on a Dir that links no files, is
  framed in a copy without its data
status: To Do
assignee: []
created_date: '2026-11-15 10:38'
labels:
  - bug
  - storage
dependencies: []
priority: low
ordinal: 820000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found reading frame_segment for TASK-411.23 (crates/extent_store/src/segments.rs): when the look at a segment's parts says it saves too little (`sampled`), its parts are never packed (`packed` stays empty) and the packer is rebuilt of raw parts; if `frame_in_place` then returns None because the Dir's `link` is unsupported (the default of `Dir::link`), the fallback writes `packed` (nothing) at 0 and the tail at `end`: a framed file whose table names parts that are holes. Every real Dir (HostDir, MemDir, the tests' wrappers) implements link, so nothing hits it today; a Dir without link (the durability Probe, a future one) would frame such segments wrong. Clear and sealed stores alike.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A segment framed in place on a Dir whose link is unsupported is framed in a file of its own whose data reads back, tested with a Dir that refuses link
- [ ] #2 Or Dir::link is required of every Dir and the fallback removed
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
