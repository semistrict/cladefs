---
id: TASK-533
title: A contiguous memory-cell read issues a device read for each 4 KiB block
status: To Do
assignee: []
created_date: '2026-11-18 12:25'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 923000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of revision 845a6c123, finding 18.

The first-touch mapper calls `FileSet::read` to warm a node before mapping its runs at `crates/cladefs/src/memory_cell.rs:382`. The read loop at `crates/extent_files/src/pages.rs:578` awaits each unmapped block separately. With metadata already available, the probe counted 170 serial device reads to fetch one contiguous 680 KiB run. TASK-456.3 deliberately installs a node ahead of subsequent faults. The avoidable work is the per-block request inside that operation. Coalesce adjacent clean blocks in the same extent while respecting device limits and dirty overrides. The count is at the Device boundary and does not imply 170 bucket GETs.

Isolated arm64 macOS validation. Commit one node of nonzero data with 4 KiB blocks. Confirm it resolves to one extent run, reset the FakeDevice read counter, then read the node and check every returned byte. 696,320 correct bytes, one run, 170 device reads. No metadata reads were necessary for this call.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A contiguous run is read in requests bounded by extent and device limits. Operation counts cover contiguous and mixed runs, with byte-for-byte verification.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
