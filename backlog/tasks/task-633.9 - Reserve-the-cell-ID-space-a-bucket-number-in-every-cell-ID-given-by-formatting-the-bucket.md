---
id: TASK-633.9
title: >-
  Reserve the cell ID space: sequential bucket numbers from the root bucket,
  recorded in every bucket
status: Done
assignee:
  - '@claude'
created_date: '2026-11-18 17:12'
updated_date: '2026-11-18 21:18'
labels:
  - bucket
  - multi-region
dependencies: []
documentation:
  - >-
    backlog/docs/doc-23 -
    Cells-across-buckets-a-bucket-per-cloud-region-forks-that-span-them.md
parent_task_id: TASK-633
priority: high
type: feature
ordinal: 942000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The user asked (2026-11-18) to reserve the ID space before the rest of TASK-633, so that IDs given from now on stay valid once buckets meet. A cell ID is `bucket << 48 | block << 24 | n` (16/24/24: a process claims a block of 2^24 IDs by creating `objectid/<block>`, then gives them with no request). Decided with the user, later the same day, superseding random numbers and a registry in host configuration: bucket numbers are sequential, from the root bucket, number 1; adding a bucket gives it the next number by a conditional write (CAS) of the root bucket's registry, which serializes additions; every bucket's root ends up with the details of every bucket (number, provider, cloud region, URL, endpoint, KMS, master key ID; never a key or a credential). The root's registry is the authority, the others copies. Numbers 0 (IDs tests name) and 65535 (u64::MAX names no cell) are never given.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Every ID an allocator gives is bucket << 48 | block << 24 | n under its bucket's number; two buckets' allocators never give one ID
- [x] #2 Formatting makes a bucket never formatted a root, number 1, with a registry recording it; an allocator formats an unformatted bucket so before its first block
- [x] #3 Adding a bucket gives the next number by a CAS on the root's registry; additions at once take numbers in turn; an addition stopped short is finished; a bucket with a number, or a root not number 1, is refused
- [x] #4 Every bucket's root holds a copy of the registry with every bucket's details, never replaced by an older one; cladefsctl format-bucket, add-bucket and buckets do and show it
- [x] #5 The collector and leak checks keep the identity and registry objects; bucket format bumped; doc-7, doc-23, CONTEXT.md and operating-a-host.md say how
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. bucket_cell::identity: the identity object (format, number, random ID), create-only; the registry (`buckets`) in the root bucket, number 1, and copies in every root; format, add (CAS on the root's registry), spread, registry.
2. objectid: ID = bucket<<48 | block<<24 | n; the allocator formats an unformatted bucket, a root, before its first block; named() for tests under number 0.
3. layout: FORMAT 20, Kind::Identity and Kind::Buckets kept by the collector and leak checks.
4. cladefsctl format-bucket, add-bucket, buckets; errnos EEXIST, EINVAL, ENOSPC.
5. Docs: doc-7, doc-23, CONTEXT.md, operating-a-host.md.
6. Tests: identity and allocator unit tests, a race and hand mutants; cladefsctl test; macOS suite; Linux suite.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-18: pushed to main as 0e7977b60 (user: push once basic unit tests pass). macOS: clippy, workspace 2628/2628, sim 41/41. Linux (ubu1) workspace suite of the earlier random-number version: all passed but two daemon tests failing with EBUSY under full-suite load (page_ins_in_flight_are_bounded, restores_over_what_a_failed_cleanup_left_fork_anew): a dropped connection's store still closing when opened_version's sync reopens it. 5 interleaved rounds of cladefsd::tests on main and the branch were clean on both; not shown to be this change. Left: a Linux suite run of 0e7977b60.

Linux (ubu1) workspace suite of main 6b7b54e24, which holds 0e7977b60: 2972/2973 passed; the one failure an exit-time SIGABRT after the test passed (tenants_hold_their_own_pages), filed apart; the EBUSY failures of earlier runs did not appear.

DoD #1: macOS workspace 2628/2628, sim 41/41, clippy; Linux (ubu1) 2972/2973 on main 6b7b54e24, the one failure an exit-time SIGABRT after a pass in an unrelated test, seen on main too, filed as TASK-650: left unchecked for that.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Cell IDs are bucket<<48 | block<<24 | n (bucket format 20), so no two buckets give one ID. Bucket numbers are sequential from the root bucket, number 1: adding a bucket (cladefsctl add-bucket) takes the next number by a CAS on the root's registry, writes the new bucket's create-only identity, and copies the registry (each bucket's number, provider, region, URL, endpoint, KMS, master key ID; no secrets) to every bucket's root, never replacing a newer copy. Formatting (cladefsctl format-bucket, or an allocator) makes an unformatted bucket a root of its own. Verified by bucket_cell unit tests (formatting, additions in turn, a racing addition, finishing a stopped one, refusals, copies never going back; hand mutants of the CAS retry and the version check killed), the cladefsctl test (format, add two, every root lists all three, a cell in bucket 2 carries 2), macOS workspace and sim suites, and the Linux suite on ubu1 (one unrelated exit-time abort, TASK-650).
<!-- SECTION:FINAL_SUMMARY:END -->
