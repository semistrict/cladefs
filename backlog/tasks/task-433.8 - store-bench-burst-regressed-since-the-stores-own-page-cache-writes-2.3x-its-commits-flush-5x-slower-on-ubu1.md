---
id: TASK-433.8
title: >-
  store-bench burst regressed since the stores' own page cache: writes 2.3x, its
  commit's flush 5x slower on ubu1
status: To Do
assignee: []
created_date: '2026-11-16 02:12'
labels: []
dependencies: []
references:
  - crates/extent_store/src/pages.rs
  - crates/cladefs-examples/examples/store-bench.rs
parent_task_id: TASK-433
priority: high
type: bug
ordinal: 877000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-433.7's A/B (docs/measurements/ubu1-log-appends-ab-2026-11-15.md), one fxjob --bench job on ubu1, ext4 on /mnt/fxdax-scratch, medians of rounds 2-4: store-bench burst (1024 writes of 256 KiB, then one commit) at 0ef6fa1b4 (TASK-433.2) wrote 297 us a write and flushed its commit in 46.6 ms; at c78d0dbf2 (main: TASK-433.1/433.3/433.5, extent_store::pages under every Dir) 690 us and 235 ms (ranges 641-720 us, 118-243 ms). durable (2 extents a commit) is unchanged (6.3 vs 5.4 ms). Unconfirmed cause; candidates: the writer throttled at pages' hard threshold, writing back on its own thread; the commit's write_back_all writing back 256 MiB of dirty pages in 1 MiB runs one after another rather than in parallel; framing reading raw files back past the cache. Other commits between the two sides could matter too: bisect on ubu1 first.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The cause named with evidence (a bisect between 0ef6fa1b4 and c78d0dbf2, and a profile or the cache's usage counters)
- [ ] #2 burst's writes and its commit's flush on ubu1 back within 10% of 0ef6fa1b4's, an A/B under fxjob --bench, each side its own target
- [ ] #3 durable unchanged (within its spread)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
