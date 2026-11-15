---
id: TASK-467
title: >-
  written-back-twice.patch mutant no longer applies after TASK-433.5
  (extent_store dir.rs): main's clippy scripts job fails again
status: Done
assignee:
  - '@claude'
created_date: '2026-11-15 20:58'
updated_date: '2026-11-15 22:12'
labels:
  - bug
  - testing
dependencies: []
priority: high
ordinal: 874000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Main CI run 43758893191 (a252cb53f): test (clippy scripts) fails: crates/cladefs/mutants/written-back-twice.patch doesn't apply. TASK-461 re-anchored it on 1cff819be; 38c8470dd (TASK-433.5, CachedDir) changed crates/extent_store/src/dir.rs again. Re-anchor the same mutation; its Killer remote::write_budgets::a_hosts_store_writes_what_its_design_says must still kill it.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Re-anchor written-back-twice.patch on CachedFile::write_back (crates/extent_store/src/cached.rs): HostFile = CachedFile<DiskFile> since 38c8470dd. Same mutation: what it wrote back written again past the cache to the file under it.
2. ubu1: the killer passes without it and crates/cladefs/mutants.sh written-back-twice kills it.
3. scripts part of scripts/test.sh passes.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Cause: HostFile became CachedFile<DiskFile> in 38c8470dd, so the patch's HostFile::write_back hunk in dir.rs had nothing left to apply to. Fix (01c98e99a): the same mutation on CachedFile::write_back in crates/extent_store/src/cached.rs: after the write-back, read_cached what was written back and write it again to the file under the cache. It now reaches MemDir's files too (CachedFile is generic); the killer is a host store's.
Evidence: ubu1 (fxjob t467-killer): the killer remote::write_budgets::a_hosts_store_writes_what_its_design_says passes without it (8.7 s); crates/cladefs/mutants.sh written-back-twice: killed, 'the kernel wrote 2263 MiB for the store, which says 783 MiB in 5109 writes: 803 MiB at most'; tree clean afterwards. Mac: CLADEFS_TEST_PARTS=scripts scripts/test.sh passes (431 script tests; every mutant set checked, 0 broken; firecracker-cargo --check).
scripts/upstream-changes.py --check passes; nothing upstream touched.

2026-11-15, coordinator: main's CI run 43765433746 on cd8d9a286 green on every job (test (workspace), macos (workspace), guest, clippy scripts, test-weakening, sandboxes among them).
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
written-back-twice.patch re-anchored on CachedFile::write_back (01c98e99a), the same mutation. ubu1: the killer passes without it, mutants.sh kills it (kernel wrote 2263 MiB vs the store's 783 MiB). macOS scripts part passes. DoD #1 awaits main's CI run 43762655434.
<!-- SECTION:FINAL_SUMMARY:END -->
