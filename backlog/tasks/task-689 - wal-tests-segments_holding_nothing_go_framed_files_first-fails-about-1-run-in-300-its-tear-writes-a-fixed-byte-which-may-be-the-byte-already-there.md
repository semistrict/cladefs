---
id: TASK-689
title: >-
  wal::tests::segments_holding_nothing_go_framed_files_first fails about 1 run
  in 300: its tear writes a fixed byte, which may be the byte already there
status: Done
assignee: []
created_date: '2026-11-20 08:14'
updated_date: '2026-11-20 08:15'
labels:
  - testing
  - flaky
dependencies: []
ordinal: 1081000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen 2026-11-20 in a Mac workspace run (left: segments 0 and 1, right: segment 0 alone). Stressed: 1 failure in 300 on main (05e3121bd) and on t3/async-1. The test tears segment 1's framed file by writing !1 (0xfe) at offset 0; its first byte is random (sealed data), so one segment in 256 is left whole and recovers.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The tear changes the byte whatever it is; the test passes 2000 stress runs
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
The test flips the byte present instead of writing a fixed one; 2000/2000 stress runs pass (main failed 1 in 300).
<!-- SECTION:FINAL_SUMMARY:END -->
