---
id: TASK-700
title: >-
  control test
  tenants_encrypted_under_the_bucket_kms_and_shredded_through_the_control_api
  fails under load: assertion at tests/control.rs:529 after 130 s
status: To Do
assignee: []
created_date: '2026-11-20 19:38'
labels: []
dependencies: []
priority: medium
ordinal: 1100000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen on ubu1 2026-11-20 in a run of cladefs --lib plus the cells, control, peers and handover tests at --test-threads 4 (TASK-698.4 branch at 2c1ec2e1-ish, nothing of it touching bucket cells or shredding): the assertion at tests/control.rs:529 (reads after ShredTenant: the shredded tenant VM read Err(EACCES), the other Ok) failed after 130 s; run alone the test passes in 116 s. The full run output kept only the panic location, not the values.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The cause found: which read gave what, and why under load
- [ ] #2 The test passes in 20 runs under the same parallel load
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
