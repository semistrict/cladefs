---
id: TASK-630
title: Page-ins in flight at a shred leave plaintext pages a second shred can't reach
status: To Do
assignee: []
created_date: '2026-11-18 15:36'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 1021000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), shred-everywhere-kms review; verified against the code by independent reviewers. Severity: low.

**Where:** `crates/cladefs/src/cache.rs`: `rg -n -F '*slot = Slot::Present(frame);' crates/cladefs/src/cache.rs`

`cache::forget` takes the scope's extents out of the map and calls `evict()`. `evict_pages` turns only `Present` slots into `Evicting` and leaves `Filling` slots as they are. A page-in already under way can still finish after that: a `read_part` on the pager, past `StoredExtent::read`'s `holders.open()`, with its store read or bucket part GET in flight, or with `data()` having opened the tail before `HostRing::forget`. When it finishes, it fills those slots with decrypted frames (`Slot::Filling` -> `Present`). That CachedExtent is no longer in the cache's map. It stays alive through the halted store's window mapping (`mapping.source`) and the fault waiting in `bring_in`. The daemon's `fault()` doesn't check halt again after its await, so it maps the page and replies, and the VM process installs it. `fetch.rs` has the same pattern: the in-flight `Fetching::part` puts the `Fetched` back into RECENT after `forget_kept`, then `data()` sets its unpacked plaintext. doc-16's fix for races is to tell the host again, but a second `ShredCell` can't reach the orphan: `cache::forget` only walks the map. So the shredded cell's plaintext stays in the tenant's arena, and can reach the guest, until the VM process drops its window. That breaks `cache::forget`'s promise that "what the stores' keys decrypted is no longer in memory, nor read in again".

**Trigger.** 1. A VM of tenant T reads cold data of cell C: page-ins fetch parts from the bucket, about 30-100 ms each. 2. The operator calls ShredCell(T,C) on the host. 3. `forget` halts the store, then `cache::forget` evicts C's extents; the slots of parts in flight are Filling and stay. Then `forget_kept` and the ring forget run. 4. The GET completes: `Fetching::part` calls `recent.put`, and `unpacked` puts plaintext in RECENT. `read_part` sets the slots Present in the orphaned CachedExtent, and `fault()` maps the page and answers. 5. A second ShredCell finds nothing in the map. The frames stay until the VM process disconnects.

**Why the tests stay green.** `remote::tests::stores_of_shredded_cells_and_tenants_are_fenced_and_leave_the_cache` and the extent_store shred tests forget with no page-in in flight. No test runs `forget` while a `Contents::read` or bucket GET is part way through.

**Backlog.** Related: TASK-411.7 (Done) built cache::forget and claims no page-in reads a forgotten extent again, but does not handle page-ins already in flight.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 When a page-in of cell C is in flight as ShredCell(T, C) runs, the evicted extent ends with no present page. The fault waiting on it gets an error, and the VM process installs no page of the shredded cell.
- [ ] #2 A test in cladefs's extent cache tests uses a Contents whose read blocks on a barrier. It starts bring_in of a page, runs cache::forget for its scope, releases the read, and asserts held() == 0 and that the waiter got an error. It goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
