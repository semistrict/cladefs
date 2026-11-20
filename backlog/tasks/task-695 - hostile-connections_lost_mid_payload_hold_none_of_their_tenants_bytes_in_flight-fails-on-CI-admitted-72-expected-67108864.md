---
id: TASK-695
title: >-
  hostile::connections_lost_mid_payload_hold_none_of_their_tenants_bytes_in_flight
  fails on CI: admitted 72, expected 67108864
status: To Do
assignee: []
created_date: '2026-11-20 13:59'
labels: []
dependencies: []
priority: high
ordinal: 1085000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
main CI run on 305aa2033 (test (workspace)): panicked at crates/cladefs/src/cladefsd/tests/hostile.rs:2430:9, 'admitted' left 72 right 67108864; log shows 'cladefsd: receiving from a VM process: unexpected end of file'. Related: TASK-548.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
