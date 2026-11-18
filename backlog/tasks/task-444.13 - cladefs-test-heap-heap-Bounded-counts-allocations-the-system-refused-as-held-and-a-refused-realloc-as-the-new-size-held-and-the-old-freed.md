---
id: TASK-444.13
title: >-
  cladefs test heap (heap::Bounded) counts allocations the system refused as
  held, and a refused realloc as the new size held and the old freed
status: Done
assignee: []
created_date: '2026-11-16 14:22'
updated_date: '2026-11-18 03:37'
labels:
  - bug
  - unsafe
dependencies: []
parent_task_id: TASK-444
priority: medium
ordinal: 880000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Bounded::check adds a request to HELD before System is asked and nothing takes it back when System returns null: a try_reserve the system refuses leaves isize::MAX counted (left 9223372036854780722, right 9011). A refused realloc also subtracts the old size, which System still holds. Each refusal moves HELD toward LIMIT (a false abort naming a case) and toward overflow of check's addition, which panics in a test build: unwinding out of a GlobalAlloc method is undefined behaviour. Found by TASK-444.4's audit (both Codex models Incorrect on heap.rs). Tests: heap::tests::a_refused_allocation_holds_nothing, a_refused_growth_holds_what_it_held (red).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A refused alloc, alloc_zeroed or realloc leaves held() as it was
- [x] #2 heap::tests pass
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Bounded counts a request first and takes it back when System refuses it (Bounded::counted); realloc subtracts the old size only on success. Verified: heap::tests::a_refused_allocation_holds_nothing and a_refused_growth_holds_what_it_held red before (9223372036854780722 held, 9011 expected), green after on macOS and ubu1. Pushed f2423c416, main 558c354e2.
<!-- SECTION:FINAL_SUMMARY:END -->
