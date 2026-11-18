---
id: TASK-633.9
title: >-
  Reserve the cell ID space: sequential bucket numbers from the root bucket,
  recorded in every bucket
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18 17:12'
updated_date: '2026-11-18 20:24'
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
- [ ] #1 Every ID an allocator gives is bucket << 48 | block << 24 | n under its bucket's number; two buckets' allocators never give one ID
- [ ] #2 Formatting makes a bucket never formatted a root, number 1, with a registry recording it; an allocator formats an unformatted bucket so before its first block
- [ ] #3 Adding a bucket gives the next number by a CAS on the root's registry; additions at once take numbers in turn; an addition stopped short is finished; a bucket with a number, or a root not number 1, is refused
- [ ] #4 Every bucket's root holds a copy of the registry with every bucket's details, never replaced by an older one; cladefsctl format-bucket, add-bucket and buckets do and show it
- [ ] #5 The collector and leak checks keep the identity and registry objects; bucket format bumped; doc-7, doc-23, CONTEXT.md and operating-a-host.md say how
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. bucket_cell::identity: create-only identity object (format, number, random id); format(bucket, number) random by default, explicit as option; refuses other numbers, reserved numbers and other formats.
2. objectid: ID = bucket<<48 | block<<24 | n; allocator learns its bucket number (formatting it with a random one if none) before its first block; named() for tests under number 0.
3. layout: FORMAT 20, Kind::Identity kept by the collector and leak checks.
4. cladefsctl format-bucket URL [NUMBER].
5. Docs: doc-7, doc-23, CONTEXT.md, operating-a-host.md.
6. Tests: unit tests of identity and allocator; cladefsctl test; macOS suite; Linux workspace suite on ubu1.
<!-- SECTION:PLAN:END -->
