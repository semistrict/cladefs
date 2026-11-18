---
id: TASK-528
title: Unexpected local cache errors turn into bucket traffic
status: To Do
assignee: []
created_date: '2026-11-18 12:25'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 918000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of revision 845a6c123, finding 13.

In `crates/bucket_cell/src/fetch.rs:998` and line 1060, local part and tail read errors are logged and treated as misses. A persistent I/O failure can incur a GET each time the in-memory copy is absent. Propagate unexpected errors while preserving genuine misses and CRC eviction. Inject local EIO and assert that it reaches the caller without a GET.

Evidence is from source inspection. Runtime reproduction is outstanding.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Injected unexpected cache I/O errors propagate without a bucket GET. A genuine miss and recovery from a CRC eviction still fetch the object.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
