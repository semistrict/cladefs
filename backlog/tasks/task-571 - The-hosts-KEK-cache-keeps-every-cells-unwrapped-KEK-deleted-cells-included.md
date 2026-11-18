---
id: TASK-571
title: 'The host''s KEK cache keeps every cell''s unwrapped KEK, deleted cells included'
status: To Do
assignee: []
created_date: '2026-11-18 15:27'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 962000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), bucket-write review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/bucket_cell/src/keys.rs`: `rg -n -F 'self.takings.lock().unwrap().insert((cell, kek.generation), (taking, ready));' crates/bucket_cell/src/keys.rs`

`CachingKms` is one per process (`cladefs::bucket::kms`, the `POOLED` OnceLock). Its `takings` map, keyed by (cell, generation), gains an entry:
- for every cell this host makes: `keys::make`, `Inheriting::made` for every fork's child, `kept`;
- for every cell it opens: `CachingKms::kek`, line 806.

Entries are removed only by `revoked(tenant)`, `forget(Scope::Cell)` or a failed unwrap. Closing a store, deleting a cell (DropCell or delete) or a cell going dead never removes its entry.

Each entry holds a `Shared` future's inner state (notifier, result, Error), an `Arc<Secret>` with the cell's unwrapped KEK, and the map slot: a few hundred bytes. None of it is counted by the governor's memory budget. With "hundreds of forks per host" and sandbox churn, the map grows linearly with every cell ever forked on the host, for the life of the daemon. It also keeps key material of long-deleted cells in memory, which doc-16's "zeroized on drop" assumes gets dropped.

**Trigger.** 1. cladefsd serves a sandbox workload: zygote restores fork a base into new cells continuously (`Uploader::forked` → `Inheriting::made` → `CachingKms::made(child, None, ..)`). Each sandbox is dropped minutes later (DropCell/delete).
2. At 10 forks/s, about 864k entries a day accumulate in `takings` (roughly 200-300 MB a day of heap outside every budget), plus one per reopened cell's taking.
3. No path frees them short of revoking or forgetting the tenant.

**Why the tests stay green.** The keys tests count KMS calls (`Counted`) and pool sizes, never the size of `takings`. Daemon tests are short-lived. Memory budgets track arenas, the store page cache and decoded states (`Decoded::held`), not the KMS cache.

**Backlog.** Same defect as F40; file once. New: TASK-411.3, TASK-411.4 and TASK-411.7 (Done) built the cache, revoked and forget, which evict only on a shred or a revocation.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 After 1,000 cells are made (keys::make or inheriting(..).made()), taken once, and every CellKeys and Ring dropped, the cache holds none of their unwrapped cell KEKs (a cached() count returns to 0, or stays under a fixed bound).
- [ ] #2 Deleting or dropping a cell, or closing its last store, takes its cell KEK out of daemon memory, while a cell still open keeps one KMS unwrap per taking.
- [ ] #3 A keys.rs test reproducing the 1,000 cells with a cached() count goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
