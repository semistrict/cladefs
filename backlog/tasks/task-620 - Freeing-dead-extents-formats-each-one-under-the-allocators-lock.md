---
id: TASK-620
title: Freeing dead extents formats each one under the allocator's lock
status: To Do
assignee: []
created_date: '2026-11-18 15:34'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 1011000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), fxfs-alloc review; verified against the code by independent reviewers. Severity: low.

**Where:** `crates/fxfs/src/object_store/allocator.rs`: `rg -n -F 'for (extent, dead) in self.inner.lock().extents.freeing_described() {' crates/fxfs/src/object_store/allocator.rs`

`freeing_described()` builds one `format!("{dead:?}")` String for every extent in the batch while the allocator's inner lock is held. The MutexGuard is a temporary in the `for` loop's iterator expression, so it lives until the loop ends, and the lock is also held across a `device.is_open` call per extent (the guest's takes OpenExtents' mutex). The guest device's `free_extents` already runs `check_free` on every extent, which logs the same TASK-121 condition. During that time every commit that applies allocator mutations, and every allocate and reserve, waits for the lock.

**Trigger.** Removing a large tree (for example 100k files, each with its own or a shared extent) within one lazy-flush interval. The next device flush calls `did_flush_device` → spawned `free_dead`, which formats about 100k Strings and does 100k `is_open` lookups under the allocator lock, roughly 50–100 ms in the guest. Concurrent writes and commits stall for that long on every such flush.

**Why the tests stay green.** Correctness tests check what is freed, not how long the lock is held. No test measures commit latency during a large free batch.

**Backlog.** RELATED: TASK-121 (Done) added this diagnostic, which the guest device's check_free already logs; TASK-79.22 (Done) keeps the batch in freeing. Neither names lock scope or formatting cost.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 free_dead formats a death description only for an extent found open, and runs no is_open check while holding the allocator's lock: a batch of 100k dead extents makes no per-extent String allocation.
- [ ] #2 While free_dead frees 100k dead extents, another task takes the allocator's lock within the time of the batch's O(n) partition, not of per-extent formatting and lookups.
- [ ] #3 A regression bench or test kills 100k extents, triggers free_dead and measures the lock wait from another task; it goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
