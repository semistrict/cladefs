---
id: TASK-645
title: >-
  object-stores CI job: the RustFS tests fail EACCES since the bucket KMS needs
  CLADEFSD_BUCKET, which the job doesn't set
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18 20:37'
updated_date: '2026-11-19 22:30'
labels:
  - testing
  - ci
dependencies: []
priority: high
type: bug
ordinal: 1029000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
CI runs 44353952388 (8111fb784) and 44380042393 (ca72b5814) on main, 2026-11-18: object-stores ("The S3 tests, against RustFS") fails in both. bucket::tests::rustfs_cells_survive_their_hosts_loss (bucket.rs:2482), cladefs::cells rustfs_places_and_syncs_stores (tests/cells.rs:152) and rustfs_forks_and_takes_cells (tests/cells.rs:39) each unwrap EACCES: "tenant 0: the KMS failed: CLADEFS_MASTER_KEY: the bucket KMS keeps tenants' KEKs in the host's bucket: CLADEFSD_BUCKET names none". The tests reach the bucket KMS without a host bucket configured, in CI's environment only (they pass where CLADEFSD_BUCKET or another KMS is set).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The four RustFS tests pass in main's object-stores CI job.
- [ ] #2 Each RustFS test sets up the KMS it relies on itself, so it doesn't depend on the job's environment and passes when run alone with only the RustFS endpoint set.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
