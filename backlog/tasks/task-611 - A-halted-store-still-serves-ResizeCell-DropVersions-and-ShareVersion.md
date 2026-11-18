---
id: TASK-611
title: 'A halted store still serves ResizeCell, DropVersions and ShareVersion'
status: To Do
assignee: []
created_date: '2026-11-18 15:33'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 1002000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), cells review; verified against the code by independent reviewers. Severity: low.

**Where:** `crates/cladefs/src/open_cell.rs`: `rg -n -F 'self.store.set_capacity(capacity).map_err(errno)' crates/cladefs/src/open_cell.rs`

`crate::halt` promises that a store halted by a panic (EIO) or a shred (EACCES) "serves nothing more". Guest requests are refused at dispatch in cladefsd server.rs, and `write`, `free`, `commit_with`, `map`, `make_version` and `fork_quiescent` check the halt through its locks. But `OpenCell::set_capacity` checks only `may_resize`, then commits and flushes a capacity commit in the local log. `drop_versions` and `share_version` check only `may_manage`, then swap the cell's head in the bucket. The control API reaches all three on a connection's halted store: ResizeCell goes through `cell_requests::resize`, the connection's `Resizes::resize`, `resize_held`, `Live::set_capacity` and `OpenCell::set_capacity`; DropVersions and ShareVersion go through `cells::with_store`, `open_by_connection` and the OpenCell method. None of these looks at the halt. So a halted store, a shredded tenant's included, still writes to its log and its bucket.

**Trigger.** 1) A guest's store is halted, either by a panic under one of its locks (`CellGuard` drop) or by `halt_stores` after its tenant's KEK is revoked or its cell shredded (EACCES). 2) The VM manager calls ResizeCell on it. `writer_or_hold` returns the connection, `resize_held` calls `store.capacity()` and then `set_capacity`, and `LocalCell::set_capacity` commits a capacity record and flushes it; on a grow, the guest is also told the new capacity. Likewise DropVersions or ShareVersion on the halted store's path swaps the head in the bucket.

**Why the tests stay green.** `open_cell::tests::a_panic_holding_a_lock_halts_the_store` lists the operations that must fail with EIO (read, write, free, commit, map, unmap, window_mappings, is_quiescent, take_over), but leaves out set_capacity, drop_versions and share_version. The cladefsd halt tests (`a_panic_holding_a_stores_lock_halts_it_alone`, `a_panic_past_a_stores_locks_halts_it`) exercise guest requests, not control API calls.

**Backlog.** Related. TASK-153.16 (Done) decided the halt and tested guest operations only; TASK-411.7 (Done) halts stores on a shred without naming control API calls.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 On a store halted by a panic (EIO) or a shred or KEK revocation (EACCES), ResizeCell, DropVersions and ShareVersion fail with the halt's errno; the store's last_commit and the cell's head are unchanged, and the guest is told no new capacity.
- [ ] #2 a_panic_holding_a_lock_halts_the_store and the shred-halt test assert this for set_capacity, drop_versions and share_version, and go red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
