---
id: TASK-579
title: >-
  The daemon's KEK cache grows by one entry per cell it ever made, forked or
  took
status: To Do
assignee: []
created_date: '2026-11-18 15:28'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 970000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), crypto review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/bucket_cell/src/keys.rs`: `rg -n -F 'takings: Mutex<Takings>,' crates/bucket_cell/src/keys.rs`

The daemon's CachingKms is process-wide (cladefs::bucket::POOLED). Entries are inserted by `made()` (every keys::make root, every Inheriting::made fork child, every taking that gave a KEK, every rotate) and by `kek()` (every taking or reading of a cell made elsewhere). They are removed only by `revoked(tenant)`, `forget(Scope::Cell)`, or a failed unwrap. Closing a store, deleting a cell, or a fork's child going never removes its entry. With sandboxes forking a new cell per VM (hundreds of forks per host, continuous zygote restores), the map holds a Shared future (finished, with its Arc<Secret>) and key for every cell ever served: about 200-300 B each, outside the governor's budget. That grows unboundedly with uptime (e.g. 1000 forks/h -> ~720K entries/month -> ~200 MB), and it keeps plaintext cell KEKs of long-deleted cells in daemon memory for the process lifetime.

**Trigger.** cladefsd running zygote restores: each restore -> host_cell::fork -> Uploader::forked -> Inheriting::made -> CachingKms::made(child, None, ..) inserts (child, 1). The VM ends and its cell is deleted (DeleteCell / lifecycle::delete); nothing removes the entry. Repeat for every sandbox; heap grows linearly with the number of cells ever forked.

**Why the tests stay green.** keys tests count KMS calls (one unwrap per taking), which the cache is for. No test measures CachingKms's size after stores close or cells are deleted, and bench_fork_extent_memory (TASK-471) checks store heap, not the KMS cache.

**Backlog.** Same defect as F32; file once. Related: TASK-443.2 (In Progress) is per-fork daemon heap outside the governor from extent maps, and its bench reopens the same cells so cannot see this; TASK-411.7 (Done) evicts only on a shred.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 After N cells are made through keys::make and inheriting on one CachingKms and their CellKeys and stores dropped (or the cells deleted), the takings entry count returns to 0.
- [ ] #2 Under continuous fork-and-delete churn (zygote restores), the cache's size stays bounded by the cells open now, not the cells ever served, and no deleted cell's unwrapped cell KEK stays in daemon memory.
- [ ] #3 A keys test with a pub(crate) cached() count reproducing that goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
