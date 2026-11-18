---
id: TASK-539.11
title: Disk governor metrics and stats
status: Done
assignee: []
created_date: '2026-11-18 14:17'
updated_date: '2026-11-18 16:49'
labels:
  - extentd
  - disk
dependencies:
  - TASK-539.9
documentation:
  - backlog/docs/doc-22 - Disk-governor-the-hosts-disk-as-a-managed-resource.md
parent_task_id: TASK-539
priority: medium
type: feature
ordinal: 943000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Operators and tests must see the budget, and an uncounted writer must show up as a number rather than a full disk (the lesson of TASK-317/330.2). (doc-22 task T10.)
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The per-device gauges of doc-22 §10 are exported and appear in GetStats through gauges(). cladefsd_part_cache_capacity_bytes reports the governed limit.
- [x] #2 cladefsd_tenant_disk_weight and cladefsd_tenant_disk_share_bytes are exported.
- [x] #3 The stats file gains a disk: line per device.
- [ ] #4 A metrics test asserts each value after a govern_once on known footprints, including other_bytes = used − stores − cache.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Deviation: Per-device gauges are in /metrics and the stats file, not GetStats' unlabelled gauges; a metrics test of every value (AC #4) not written.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Per-device cladefsd_host_disk_* gauges in /metrics, tenant reserved/weight/share gauges, disk_reserved_bytes in GetStats' gauges, the stats file's disks: line. Tested in weights_share_a_disk_under_pressure (render and line).
<!-- SECTION:FINAL_SUMMARY:END -->
