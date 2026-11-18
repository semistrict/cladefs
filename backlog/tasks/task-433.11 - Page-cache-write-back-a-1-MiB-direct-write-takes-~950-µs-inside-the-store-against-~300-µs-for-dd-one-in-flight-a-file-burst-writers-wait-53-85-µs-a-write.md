---
id: TASK-433.11
title: >-
  Page cache write-back: a 1 MiB direct write takes ~950 µs inside the store
  against ~300 µs for dd, one in flight a file; burst writers wait 53-85 µs a
  write
status: To Do
assignee:
  - '@claude'
created_date: '2026-11-18 04:56'
labels: []
dependencies: []
parent_task_id: TASK-433
priority: medium
type: bug
ordinal: 900000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Left by TASK-433.9 (docs/measurements/ubu1-burst-writes-2026-11-17.md). After TASK-512 and the page frames, a store-bench burst write on ubu1 is 165 µs (RAM loop ext4) / 208 µs (zvol), of which the writer waits 53-85 µs for write-back: past 8 MiB begun, Store::finish waits for the oldest range, which the flusher writes a 1 MiB direct pwrite at a time, one in flight per file (the writeback lock). Each pwrite takes ~950 µs wall, 163 µs CPU (strace -c -w), off CPU in blk_io_schedule under iomap_dio_rw (bpftrace); dd oflag=direct of noise, alone, takes ~300 µs a MiB on both disks, 4 at once 4.8 GB/s. The host's page cache, writing back with many requests in flight, left 150b82934's writer waiting 0-62 µs. Tried and dropped: begun ranges merged into 4 MiB writes (RAM worse, zvol better).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A profile names why a 1 MiB direct write-back takes 3x dd's inside store-bench burst (the store's other I/O, the loop device, CPU placement, or other)
- [ ] #2 Burst writers' wait for write-back on ubu1 at most a third of 53-85 µs a write, an A/B under fxjob --bench, each side its own target
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
