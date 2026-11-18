---
id: TASK-533
title: A contiguous memory-cell read issues a device read for each 4 KiB block
status: In Progress
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
- [x] #1 A contiguous run is read in requests bounded by extent and device limits. Operation counts cover contiguous and mixed runs, with byte-for-byte verification.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
The storage audit counted device operations during first touch. Byte comparisons and single-page path counts missed serial reads within a contiguous run. The file_operations regression checks the complete node and its device read count. The read loop groups physically consecutive blocks under the transfer size used by extent writes. Dirty pages, punched holes, recently written pages and mapped pages retain their own read sources. The existing one-page helper still serves partial-page writes and compaction.
An independent macOS probe read an unaligned 600-page span containing a dirty page and a punched page. It verified all bytes and counted five device requests across the three clean runs, including transfer-limit splits.

The unchanged regression failed in the macOS audit run and passed against the implementation. Both implementation reviews reported no findings.
The regression also passed on ubu1 in run r516e against 24c6081a6 with the production patch. All 19 extent_files tests passed on both platforms. The macOS crate run passed 412 of 413 (TASK-530 failed), and its workspace run stopped on TASK-521. The ubu1 crate run passed 681 of 684 (TASK-519, TASK-520 and TASK-522 failed, as in the baseline audit). Workspace qualification remains open.
<!-- SECTION:NOTES:END -->
