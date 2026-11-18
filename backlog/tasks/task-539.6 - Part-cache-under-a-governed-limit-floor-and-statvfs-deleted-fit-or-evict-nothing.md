---
id: TASK-539.6
title: >-
  Part cache under a governed limit: floor and statvfs deleted, fit or evict
  nothing
status: To Do
assignee: []
created_date: '2026-11-18 14:16'
labels:
  - extentd
  - disk
dependencies:
  - TASK-539.1
documentation:
  - backlog/docs/doc-22 - Disk-governor-the-hosts-disk-as-a-managed-resource.md
parent_task_id: TASK-539
priority: medium
type: feature
ordinal: 938000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Requirement (3). Today the cache gives space back only on its next fill. Concurrent fills each pass the floor (TASK-521), and a fill that cannot fit empties the cache (TASK-443.8). (doc-22 task T6.)
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Cache::open(dir, limit) and Cache::set_limit(bytes) -> io::Result<u64> exist. The capacity and floor fields, Files::free, make_room's free and needed, and cells.rs CACHE_FLOOR are deleted.
- [ ] #2 make_room(limit, len) chooses victims first and applies them only when the fill fits. A test fills the cache and then asks for a part that cannot fit: nothing is evicted (TASK-443.8).
- [ ] #3 audit_tests.rs free_space_admission_reserves_space_for_unwritten_fills is restated against a limit with room for one fill and passes (TASK-521).
- [ ] #4 Every Index.bytes change moves the HostDisk's cache counter; fills in flight move cache_writing until settle; tails count rounded to ALIGN.
- [ ] #5 adopt evicts least recently read data to fit and refuses only when len > limit. adoptions_past_the_capacity_or_the_floor_are_refused, keeps_the_floor_free and capacity_and_floor_hold_across_restarts are rewritten for the limit and pass.
- [ ] #6 crashes_at_each_change_leave_nothing_read_back_wrong passes. CLADEFSD_PART_CACHE_BYTES still caps the cache (the_cache_takes_its_budget passes).
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
