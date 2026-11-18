---
id: TASK-509
title: >-
  pages: a_dead_flusher_is_known_and_holds_no_writer_for_good aborts at times,
  the dying flusher finding the holding lock poisoned
status: To Do
assignee: []
created_date: '2026-11-18 00:59'
labels:
  - extent_store
  - flaky
dependencies: []
ordinal: 902000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen 2026-11-18 in a whole-workspace nextest on macOS (branch t3/cell-is-one-filesystem, pages.rs unchanged from origin/main): SIGABRT after the test passed. The test's thread panics in PageCache::throttle (pages.rs:368, the flusher died) while holding `holding`, poisoning it; the flusher, unwinding from its own panic (pages.rs:1264, the test's Panicking file), reaches PageCache::drained, whose lock().unwrap() (pages.rs:379) panics on the PoisonError while panicking, which aborts the process. 8 reruns alone passed. Likely fix: throttle's assert before it takes the lock, or drained tolerating a poisoned lock.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
