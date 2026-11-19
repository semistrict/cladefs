---
id: TASK-633.3
title: >-
  Hosts find buckets by number: the registry in their bucket's root, credentials
  per bucket
status: Done
assignee:
  - '@claude'
created_date: '2026-11-18 17:03'
updated_date: '2026-11-19 00:16'
labels:
  - bucket
  - multi-region
dependencies:
  - TASK-633.2
  - TASK-633.9
documentation:
  - >-
    backlog/docs/doc-23 -
    Cells-across-buckets-a-bucket-per-cloud-region-forks-that-span-them.md
parent_task_id: TASK-633
priority: high
type: feature
ordinal: 934000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
With TASK-633.9 every bucket has a sequential number and every bucket's root holds the registry of every bucket (the root bucket's the authority). A host that reaches another bucket than its own, for a parent's extents or a fork across buckets, needs that bucket by number: its URL and details from the registry copy in its own bucket's root, or from the root bucket's for a number the copy lacks; and its credentials and per-host options, which the registry never holds, from the host's configuration by bucket number.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A host resolves a bucket number to its bucket from its own root's copy, reading the root bucket's registry for a number the copy lacks
- [x] #2 Credentials and options per bucket come from the host's configuration by number, none from the registry
- [x] #3 A bucket whose identity disagrees with the registry's entry for its number is refused
- [x] #4 operating-a-host.md says how a host is configured for several buckets
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. bucket_cell: identity::verified (an entry against the identity found), Error::NoBucket and Error::Mismatched; object_bucket::numbered_options and ObjectBucket::from_url_numbered (CLADEFSD_BUCKET_<N>_<NAME> over NAME).
2. cladefs host_cell::bucket_numbered(home, number): home itself; else the entry in home's registry copy, or the root bucket's registry for one the copy lacks; opened with its options, identity verified; cached per (home, number). cladefs::bucket::bucket_numbered over CLADEFSD_BUCKET; errnos ENOENT, ESTALE.
3. Tests: hosts_find_buckets_by_number (own, by copy, by the root for a stale copy, mismatch refused, unknown refused), numbered_options; a hand mutant of the identity check.
4. operating-a-host.md.
<!-- SECTION:PLAN:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
A host finds a bucket by number: its own, else the registry copy in its root, else the root bucket's registry; opened with its own options (CLADEFSD_BUCKET_<N>_<NAME> over NAME; the registry holds no secrets) and refused unless its identity is the one recorded (ESTALE), a number no registry records being ENOENT. Since TASK-633.4 it goes through the router (ObjectBucket::numbered, the Registered resolver). Verified by hosts_find_buckets_by_number (own, by copy, by the root for a stale copy, mismatch and unknown refused; a mutant dropping the identity check fails it), numbered_options_take_their_buckets_own_variables_first, and the macOS and Linux suites (ubu1 2979/2979 at 2a7477fed).
<!-- SECTION:FINAL_SUMMARY:END -->
