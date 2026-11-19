---
id: TASK-673
title: >-
  go/: peer.pb.go out of date with peer.proto since TASK-537.3.5 (6800f5cd0):
  DropLogsRequest's comments; test (go) red on main's CI
status: To Do
assignee: []
created_date: '2026-11-19 22:27'
labels:
  - ci
  - bug
dependencies: []
priority: medium
ordinal: 1055000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
main's CI job test (go) fails since run 44462208276 (a296e3ee2) and still on 28a38a315 (run 44615661631): 'go/: generated code out of date with the .proto: run go/generate.sh'. The diff is DropLogsRequest's doc comments (Of: 'without through, every configuration's before it dropped'; Through: 'its logs to it dropped, and nothing of another configuration'), changed in peer.proto by 6800f5cd0 (TASK-537.3.5) without regenerating go/peer/v1/peer.pb.go.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 go/generate.sh run; go/peer/v1/peer.pb.go matches peer.proto
- [ ] #2 CI's test (go) passes on main
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
