---
id: TASK-633.3
title: >-
  Bucket numbers: cell IDs unique across buckets, a bucket identity object, the
  operator registry
status: To Do
assignee: []
created_date: '2026-11-18 17:03'
labels:
  - bucket
  - multi-region
dependencies:
  - TASK-633.2
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
Cell IDs are unique only in their bucket (`objectid.rs`: an ID is `block << 24 | n`), and Origin, Location, ExtentId, KeyId, reference keys and the extent cache name cells by that bare ID, so two buckets may name different cells alike and the extent cache would serve one's pages for the other's silently. The user chose (2026-11-18) a 16-bit bucket number in a cell ID's high bits, given by the operator in a registry in each host's configuration. A bucket URL is no identity (`s3://b` and its regional https form are one bucket). Format bump, no migration.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A cell ID is bucket << 48 | block << 24 | n; the allocator takes blocks within its bucket's number; number 0 is never given
- [ ] #2 Every bucket holds a create-only identity object (format, number, random ID, cloud, cloud region), made on first use; a host refuses a bucket whose object disagrees with its registry
- [ ] #3 The registry maps each bucket number to its URL and options; CLADEFSD_BUCKET names the home bucket by number; operating-a-host.md and CONTEXT.md (bucket number, home bucket, registry) say so
- [ ] #4 Tests: two buckets' allocators never give one ID; a mismatched identity object is refused
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
