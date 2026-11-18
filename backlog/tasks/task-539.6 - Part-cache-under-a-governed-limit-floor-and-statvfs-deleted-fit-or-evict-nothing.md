---
id: TASK-539.6
title: >-
  Part cache under a governed limit: floor and statvfs deleted, fit or evict
  nothing
status: Done
assignee: []
created_date: '2026-11-18 14:16'
updated_date: '2026-11-18 16:49'
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
- [x] #1 Cache::open(dir, limit) and Cache::set_limit(bytes) -> io::Result<u64> exist. The capacity and floor fields, Files::free, make_room's free and needed, and cells.rs CACHE_FLOOR are deleted.
- [x] #2 make_room(limit, len) chooses victims first and applies them only when the fill fits. A test fills the cache and then asks for a part that cannot fit: nothing is evicted (TASK-443.8).
- [x] #3 audit_tests.rs free_space_admission_reserves_space_for_unwritten_fills is restated against a limit with room for one fill and passes (TASK-521).
- [ ] #4 Every Index.bytes change moves the HostDisk's cache counter; fills in flight move cache_writing until settle; tails count rounded to ALIGN.
- [x] #5 adopt evicts least recently read data to fit and refuses only when len > limit. adoptions_past_the_capacity_or_the_floor_are_refused, keeps_the_floor_free and capacity_and_floor_hold_across_restarts are rewritten for the limit and pass.
- [x] #6 crashes_at_each_change_leave_nothing_read_back_wrong passes. CLADEFSD_PART_CACHE_BYTES still caps the cache (the_cache_takes_its_budget passes).
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Deviation: AC #4's 'tails count rounded to ALIGN' not done: the cache counts its objects' bytes as before, at most a block under per object.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Cache::open(dir, budget), set_limit (governed), make_room chooses victims then evicts only if the fill fits; counts on HostDisk.cache and cache_writing. Tests: a_governed_limit_evicts_down_to_it_at_once, fills_that_cant_fit_evict_nothing (TASK-443.8), the TASK-521 audit test now passing, budgets_and_limits_hold_across_restarts, adoptions_past_the_limit_are_refused. Linux (ubu1, fxjob, 9d5012c..2d67074): clippy --workspace --tests clean; local_cell+bucket_cell 590/593, cladefs 666/669, rest of workspace 1680/1686, every failure one of a1167a56a's regression tests for open bugs (TASK-516..536), failing on the base too. macOS: the same crates' suites, same three pre-existing audit failures.
<!-- SECTION:FINAL_SUMMARY:END -->
