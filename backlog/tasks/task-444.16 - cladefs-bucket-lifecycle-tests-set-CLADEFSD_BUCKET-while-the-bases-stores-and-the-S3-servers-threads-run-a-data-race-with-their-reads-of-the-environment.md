---
id: TASK-444.16
title: >-
  cladefs bucket lifecycle tests set CLADEFSD_BUCKET while the base's stores'
  and the S3 server's threads run: a data race with their reads of the
  environment
status: Done
assignee: []
created_date: '2026-11-18 01:06'
updated_date: '2026-11-18 03:37'
labels:
  - bug
  - unsafe
dependencies: []
parent_task_id: TASK-444
priority: medium
ordinal: 897000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
lifecycle.rs's host() called std::env::set_var("CLADEFSD_BUCKET") after Made::new had made and opened the base's stores, whose uploads run on threads, and, in the S3 tests, after the server's tokio runtime started: set_var is undefined behaviour while another thread may read the environment (getenv), its contract. TASK-444.4 had moved it before any thread in the old lifecycle; main's restructure (Made, host()) put it back after. Found by TASK-444.4's deny(clippy::undocumented_unsafe_blocks) at the merge: no true SAFETY comment could be written. Fix: the host's bucket URL a test sets without the environment (host_store::use_host_bucket, read first by host_bucket_url).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 No test sets CLADEFSD_BUCKET while its process may have other threads
- [x] #2 lifecycle's tests pass
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
host() makes the host's bucket with host_store::use_host_bucket, which host_bucket_url reads before CLADEFSD_BUCKET; no environment change while threads run. Verified: bucket::lifecycle tests pass on macOS and ubu1; clippy with deny(undocumented_unsafe_blocks) passes on both. Pushed db3dfc36d, main 558c354e2.
<!-- SECTION:FINAL_SUMMARY:END -->
