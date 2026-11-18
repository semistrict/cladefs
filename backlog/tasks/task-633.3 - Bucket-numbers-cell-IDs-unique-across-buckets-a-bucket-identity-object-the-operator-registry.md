---
id: TASK-633.3
title: >-
  Hosts find buckets by number: the registry in their bucket's root, credentials
  per bucket
status: To Do
assignee: []
created_date: '2026-11-18 17:03'
updated_date: '2026-11-18 20:24'
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
- [ ] #1 A host resolves a bucket number to its bucket from its own root's copy, reading the root bucket's registry for a number the copy lacks
- [ ] #2 Credentials and options per bucket come from the host's configuration by number, none from the registry
- [ ] #3 A bucket whose identity disagrees with the registry's entry for its number is refused
- [ ] #4 operating-a-host.md says how a host is configured for several buckets
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
