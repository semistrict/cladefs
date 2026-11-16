---
id: TASK-433.7
title: >-
  Log appends make one O_DIRECT write and one ftruncate per record since
  TASK-433.2: measure, then batch a flush into one write
status: Done
assignee:
  - '@claude'
created_date: '2026-11-15 13:56'
updated_date: '2026-11-16 03:25'
labels: []
dependencies: []
references:
  - crates/extent_store/src/wal.rs
  - crates/extent_remote/src/direct.rs
parent_task_id: TASK-433
priority: medium
type: bug
ordinal: 862000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the verifying-reviewer agent (TASK-444.9) on 0ef6fa1b4; unconfirmed until measured. SegmentLog::append (wal.rs) writes record by record, then syncs once; logs open with data = false, so each HostFile::write_at is direct::write_at: an fstat, an O_DIRECT write waiting on the device and extending i_size to the block end, then set_len back when the record ends mid-block (nearly always). Before, N records were N page-cache copies and one fdatasync; now N device round trips plus N extending writes and truncates, each an ext4 journal transaction on the orphan list (s_orphan_lock, one per file system, shared by every store on the host). TASK-401 measured ~0.5 ms a direct write on a network disk, the reason TASK-433.1 and 433.3 were held back. The commit reports pass/fail only, no timings.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 An A/B on ubu1 ext4 under fxjob --bench, 0ef6fa1b4 against its parent, each side its own target: store-bench durable (one record a flush) and burst (many), per-flush latency recorded
- [ ] #2 If burst latency grows with records per flush: an append writes its records as one direct write with at most one set_len, and the A/B is rerun
- [x] #3 If it does not: the notes say so and the task closes without a change
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. ubu1, three sides each in its own worktree and target: A 39f34921c (parent), B 0ef6fa1b4, C c78d0dbf2 (main, with the stores' page cache); store-bench release built under fxjob.
2. One fxjob --bench job: rounds interleaved A B C / C B A, durable and burst each, TMPDIR on ext4 /mnt/fxdax-scratch.
3. If burst's commit grows B vs A (records per flush): SegmentLog::append writes its records as one write; rerun C vs fix.
4. Else note it and close without a change.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
A/B run 2026-11-15 on ubu1, one fxjob --bench job, sides A 39f34921c (parent), B 0ef6fa1b4, C c78d0dbf2 (main), each its own worktree and target, binaries checked for a side-only string (direct.rs's 'a block-aligned layout': A 0, B 1, C 1); 4 rounds interleaved, TMPDIR on ext4 /mnt/fxdax-scratch (a ZFS zvol). Medians rounds 2-4: durable flush A 5.6 / B 6.3 / C 5.4 ms; burst commit flush A 42.8 / B 46.6 / C 235.2 ms; burst write A 295 / B 297 / C 690 us. B vs A within spread (+3.8 ms over ~1024 records): no batching. AC #2 not applicable (burst latency does not grow with records per flush). C's burst regression filed as TASK-433.8. Results: docs/measurements/ubu1-log-appends-ab-2026-11-15.md.

DoD: no code changed; main at 41e2ce9af passes the workspace on macOS and ubu1 (TASK-433.6's runs); upstream check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Measured, no change: on ubu1 ext4 the per-record O_DIRECT write and set_len of TASK-433.2 cost durable flushes 5.6 -> 6.3 ms and burst's ~1024-record flush 42.8 -> 46.6 ms (medians, rounds 2-4, inside B's spread), so appends stay record by record (AC #3). The A/B also showed main regressed burst 5x since (TASK-433.8). Evidence: docs/measurements/ubu1-log-appends-ab-2026-11-15.md.
<!-- SECTION:FINAL_SUMMARY:END -->
