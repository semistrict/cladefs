---
id: TASK-504
title: 'IDs in base 32: bucket keys, store paths and displayed IDs leave hexadecimal'
status: Done
assignee:
  - '@claude'
created_date: '2026-11-17 01:49'
updated_date: '2026-11-17 03:53'
labels: []
dependencies: []
ordinal: 892000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Every u64 naming something (tenant, cell, branch, extent sequence number, segment number, epoch, version, pass) is written as 16 hexadecimal digits in bucket keys and store paths, and unpadded hex where operators read and type it (TENANT/CELL/ID, cladefsctl, cladefs-gc, metrics labels, KMS key names, control socket names). The user asked for base 32: shorter keys and names. No deployment exists, so the formats change outright with no compatibility (AGENTS.md: no migrations).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 One codec, used everywhere: fixed width 13 digits in keys and file names, sorting as the numbers do; shortest form where displayed; both refuse non-canonical text
- [x] #2 No ID is written or parsed in hexadecimal in bucket keys, store paths, CLI input and output, metrics labels, KMS key names, sockets and logs
- [x] #3 Docs and comments say base 32 where they said hexadecimal of IDs
- [x] #4 Workspace tests pass on macOS and the Linux suite on ubu1
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. crates/base32_id: Padded (13 digits, sorts as numbers) and Short forms, Crockford lower case; parse refuses anything else.
2. Bucket keys (extent_frame, extent_bucket layout), store paths and description (extent_store layout), FUSE paths, boot profile, store_dir records: Padded.
3. Displayed IDs (Branch/Cell Display and FromStr, cladefsctl, cladefs-gc, metrics, KMS key names, sockets, logs): Short.
4. Formats bumped: bucket 15, store 14, profile 2; docs, protos, Go fake, gc-sweep.sh.
5. Tests: macOS scripts/test.sh, then ubu1 test-host.sh.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
macOS scripts/test.sh: clippy, workspace (2642 passed), sim, shuttle, loom, scripts, go all pass after fixing 11 stale test expectations. vendor/firecracker.lock and guest/tests/Cargo.lock gained base32_id. Commit 3ad2bd165; ubu1 run r504b32 started.

ubu1 r504b32: workspace 2953/2954 (memory test's tenant label "ac" stale, fixed to "5c", rerun r504mem 35/35 Linux-only cladefs tests pass); guest tests all pass but fxfs-guest-checks-dax-delay's writers_on_a_full_filesystem_share_syncs, a full-filesystem race in the guest, unrelated (TASK-507). upstream-changes.py --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
IDs are base 32 everywhere they were 16-digit or short hex: new crate base32_id (Crockford, lower case; Padded 13 digits for keys and file names, sorting numerically; Short for display; strict parsing). Bucket keys (FORMAT 15), store paths and description (store format 14), boot profiles (2), FUSE paths, cladefsctl/cladefs-gc I/O, metrics labels, KMS key names and contexts, tenant sockets, logs. Docs, protos (Go pb regenerated), Go fake, gc-sweep.sh, firecracker and guest locks updated. Verified: macOS scripts/test.sh all parts green (2642 workspace tests, sim, shuttle, loom, scripts, go, mutants, firecracker lock check); ubu1 Linux workspace and guest suites green but TASK-507, unrelated.
<!-- SECTION:FINAL_SUMMARY:END -->
