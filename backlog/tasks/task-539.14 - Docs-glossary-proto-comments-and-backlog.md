---
id: TASK-539.14
title: 'Docs, glossary, proto comments and backlog'
status: Done
assignee: []
created_date: '2026-11-18 14:18'
updated_date: '2026-11-18 17:42'
labels:
  - extentd
  - disk
dependencies:
  - TASK-539.9
  - TASK-539.10
  - TASK-539.11
documentation:
  - backlog/docs/doc-22 - Disk-governor-the-hosts-disk-as-a-managed-resource.md
parent_task_id: TASK-539
priority: medium
type: docs
ordinal: 946000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The documented meaning of the disk count (du -sx, walks every 10 s) and the cache floor change, and operators need the new knobs. (doc-22 task T11.)
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 CONTEXT.md: Governor covers the disk budget; HostDisk, Footprint, Allotment, Spare and Headroom are added; 'room' stays a segment's.
- [x] #2 docs/operating-a-host.md 'The host's disk' describes the dedicated default, fixed mode, headroom as bytes or %, the backpressure order, weights, metrics, f_bavail and ZFS behaviour, with no du semantics.
- [x] #3 The proto comments for SetDiskLimit, GetTenantDiskResponse.bytes and SetDiskWeight are updated reworded in place (doc-22 decisions).
- [x] #4 doc-14's bound sentence and doc-7's stale CLADEFSD_CACHE* names are corrected through the backlog CLI. TASK-355, TASK-521 and TASK-443.8 (the evict-all part) are closed citing T4–T6.
- [ ] #5 Whether measure_disk_recounts_with_resident_pages (TASK-525) is deleted is asked of the user first
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
The TASK-525 benchmark (measure_disk_recounts_with_resident_pages) is kept, now timing count(None); deleting it is the user's call.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
CONTEXT.md: Governor covers the disk; Host disk, Footprint, Allotment, Reserve, Headroom, Spare added, room staying a segment's. docs/operating-a-host.md's disk section rewritten (dedicated default, CLADEFSD_DISK_HEADROOM/CLADEFSD_DISK_BYTES, pressure, weights, reserve and its 7x for uncommitted data, metrics, no du). Proto comments for SetDiskLimit, SetDiskWeight and GetTenantDiskResponse. cladefsd's own docs. doc-7 and doc-14 corrected, doc-22 given an as-built section, through the backlog CLI. TASK-355, TASK-521 done; TASK-443.8 noted (its measurement still open).
<!-- SECTION:FINAL_SUMMARY:END -->
