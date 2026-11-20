---
id: TASK-692
title: >-
  go/: cladefsd.pb.go out of date with cladefsd.proto since TASK-681
  (PlaceCellRequest.force, StatCellResponse.forced_after): test (go) red on
  main's CI
status: Done
assignee: []
created_date: '2026-11-20 13:58'
updated_date: '2026-11-20 14:04'
labels: []
dependencies: []
priority: medium
ordinal: 1082000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
main CI run on 305aa2033: go/generate.sh --check fails, the checked-in Go code lacks PlaceCellRequest.force and StatCellResponse.forced_after.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
go/generate.sh run; CLADEFS_TEST_PARTS='scripts go' scripts/test.sh passes on the Mac.
<!-- SECTION:NOTES:END -->
