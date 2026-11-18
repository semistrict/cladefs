---
id: TASK-590
title: A follower pins a held file's extents one at a time under the fill barrier
status: To Do
assignee: []
created_date: '2026-11-18 15:30'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 981000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), followers review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/fxfs/src/follower.rs`: `rg -n -F "self.device().pin(extent).await.context("Pinning a held object's extent")?;" crates/fxfs/src/follower.rs`

When the writer deletes a file a follower holds, `keep_deleted` collects every ExtentId the kept records name. It then sends one PIN per extent and awaits each before the next. When the hold goes, `apply` likewise sends one UNPIN per extent (`for extent in self.release_holds()? { self.device().unpin(extent).await; }`). Both run inside `FxFilesystem::apply`, which `follower::Protocol::batch` calls while holding the fill barrier exclusively. On the guest, `ExtentDevice::pin` and `unpin` are each a separate virtio request that crosses the VM process and the daemon. A file with N device extents therefore keeps every reader of the follower (lookups, stat, read(2), page-cache and DAX faults) waiting for N sequential round trips. doc-8 budgets a batch at milliseconds (measured maxima of 8 to 41 ms), but a file of 10k extents at roughly 30 µs per round trip blocks all readers for about 0.3 s, once when it is deleted and again when it is released. The pins have no ordering among themselves and only need to complete before the LET_GO that follows the barrier, so they can be sent together and awaited once.

**Trigger.** Step 1. A follower reader holds open a large file the writer wrote in many writebacks (thousands of extents), for example a log a follower is tailing. Step 2. The writer deletes or rotates it and commits. Step 3. The follower's batch takes the barrier, and `keep_deleted` sends thousands of PIN requests one after another. Every reader of the follower blocks on `cladefs_vfs_fill_begin` for that whole time. The same stall happens on the batch after the reader closes the file, this time from UNPINs.

**Why the tests stay green.** graveyard.rs pins through ViewDevice, which is an in-process mutex operation with no round trip, and makes no timing assertion. The guest torture test's held files have few extents, and its FXFS_IOC_FOLLOW_STATS figures measure how long batches wait for readers, not how long readers wait for batches.

**Backlog.** Related: TASK-42.3, TASK-42.4 and TASK-42.6 (Done) built PIN, the follower's graveyard and the guest side. TASK-42.7 (Done) measures how long batches wait for readers, not how long readers wait for a batch.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Take a follower holding a file of 20k extents. When the writer deletes the file, and again when the follower's reader closes it, a concurrent stat loop on the follower never waits more than 20 ms on the fill barrier.
- [ ] #2 The PIN requests of one batch, and its UNPIN requests, take a fixed number of awaited device round trips that does not grow with the file's extent count.
- [ ] #3 A guest case measures the longest wait of that stat loop, or counts the device round trips each batch awaits against the PINs it sends. It goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
