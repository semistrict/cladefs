---
id: TASK-475.1
title: 'Rename cladefs-extentd to cladefsd, CLADEFS_EXTENTD_* to CLADEFSD_*'
status: Done
assignee:
  - '@claude'
created_date: '2026-11-16 02:24'
updated_date: '2026-11-16 05:36'
labels:
  - host
dependencies: []
parent_task_id: TASK-475
priority: high
ordinal: 878000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
First step of TASK-475: the daemon takes its final name before the FUSE mount moves into it, so no new variable is born under the old prefix. The extentd gRPC service, its proto package and go/extentd/v1 keep their names: they name the extent-serving service, one of cladefsd's. The Rust module cladefs::extentd and Firecracker's extentd_socket keep theirs for the same reason. Records (docs/measurements, closed tasks) keep the old names, as TASK-437 kept fxdax.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The binary is cladefsd; no cladefs-extentd binary is built
- [x] #2 Every CLADEFS_EXTENTD_* variable is read as CLADEFSD_* instead, and the old names are refused at start with an error naming the new one, so a host left with old names fails loudly instead of running unconfigured
- [x] #3 Scripts, tests, README, docs/ (integrator guide, upgrading a host, tracing), the guide and the Go daemon test use the new names; rg finds the old names only in records
- [x] #4 A script renames them in a branch begun before, as scripts/rename-to-cladefs.sh does
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. scripts/rename-to-cladefsd.sh: perl rename of cladefs-extentd, CLADEFS_EXTENTD(_*), cladefs_extentd(_*) in live files (records excluded), file move, Go regenerated; --filter, --backlog (shared scripts/lib/rename_backlog.py, also used by rename-to-cladefs.sh).
2. crates/cladefs/src/renamed.rs: every cladefs binary refuses old variable names at start, naming the new ones.
3. Prefix filters that forwarded CLADEFS_* (host.sh, bench-rounds.sh, guest_results.py, lima.sh, test fakes) take CLADEFSD_* too.
4. Verify: scripts' Python tests, macOS build + unit tests, Linux clippy and the daemon-spawning tests on ubu1.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Ran the script: 0 live files with old names. Bare CLADEFS_EXTENTD (guest tests' daemon socket, Go test's binary) renamed CLADEFSD too. Gotcha found: CLADEFSD_ is not under the CLADEFS_ prefix, so every prefix filter that forwards cladefs settings to hosts needed CLADEFSD_ added (test_host caught it). macOS: build and renamed:: tests pass; scripts tests pass (test_host, test_linux_host, test_rename_to_cladefs*, test_guest_results).

Linux (ubu1): 100 cladefs tests of the daemon-spawning binaries (drain, cladefsctl, grants, isolation, handover, peers, tracing, control, disk, memory, branches, tenants, fuse_mount) + renamed::, store_table::, extentd::mount:: pass; sqlite_extents/virtio_extent daemon tests 15/15; go test -tags extentd ./... ok; clippy -p cladefs --no-deps clean but for 1.100 lints in code not touched (TASK-477). upstream-changes.py --check passes; go/generate.sh --check passes. Full workspace suites not yet run (DoD #1).

DoD #1: scripts/test.sh clippy+workspace on commit 1270ab9fe: macOS 2517/2517, Linux (ubu1, fxjob) 2827/2827; CI's clippy rules clean on both.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
cladefs-extentd renamed cladefsd, CLADEFS_EXTENTD(_*) to CLADEFSD(_*), metrics cladefsd_*; old names refused at start (renamed.rs); scripts/rename-to-cladefsd.sh for older branches; host-forwarding filters take CLADEFSD_*. Verified: rename/script tests, daemon-spawning tests and full workspace suites on macOS and Linux.
<!-- SECTION:FINAL_SUMMARY:END -->
