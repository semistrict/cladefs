---
id: TASK-433.7
title: >-
  Log appends make one O_DIRECT write and one ftruncate per record since
  TASK-433.2: measure, then batch a flush into one write
status: To Do
assignee: []
created_date: '2026-11-15 13:56'
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
- [ ] #1 An A/B on ubu1 ext4 under fxjob --bench, 0ef6fa1b4 against its parent, each side its own target: store-bench durable (one record a flush) and burst (many), per-flush latency recorded
- [ ] #2 If burst latency grows with records per flush: an append writes its records as one direct write with at most one set_len, and the A/B is rerun
- [ ] #3 If it does not: the notes say so and the task closes without a change
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
