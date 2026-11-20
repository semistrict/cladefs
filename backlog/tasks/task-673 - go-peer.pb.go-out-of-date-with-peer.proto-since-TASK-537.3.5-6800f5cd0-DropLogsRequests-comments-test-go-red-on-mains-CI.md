---
id: TASK-673
title: >-
  go/: peer.pb.go out of date with peer.proto since TASK-537.3.5 (6800f5cd0):
  DropLogsRequest's comments; test (go) red on main's CI
status: Done
assignee:
  - '@claude'
created_date: '2026-11-19 22:27'
updated_date: '2026-11-20 00:01'
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
- [x] #1 go/generate.sh run; go/peer/v1/peer.pb.go matches peer.proto
- [x] #2 CI's test (go) passes on main
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
go/generate.sh regenerated go/peer/v1/peer.pb.go (DropLogsRequest's two comments); generate.sh --check passes; go build ./... and go vet ./peer/... pass.

CI run 44629552079 (e7fbf3abb): its job passes. Workspace: macOS 2754/2754; Linux (ubu1, fxjob, with TASK-644/675's changes too) 3096/3097, the one failing mappings_asked_for_past_the_credit_let_their_vm_process_go, TASK-546's, failing on main before. DoD #1 left unchecked for that. scripts/upstream-changes.py --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
go/peer/v1/peer.pb.go regenerated from peer.proto (go/generate.sh), which 6800f5cd0 changed without regenerating. Verified: generate.sh --check, go build and vet, and main's test (go) CI job (run 44629552079).
<!-- SECTION:FINAL_SUMMARY:END -->
