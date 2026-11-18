---
id: TASK-601
title: Shredding leaves the extent cache's pages of closed stores in memory
status: To Do
assignee: []
created_date: '2026-11-18 15:31'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 992000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), host-dax review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/cladefs/src/cache.rs`: `rg -n -F '.filter(|(_, cached)| cached.holders.forget(&scope) && !cached.holders.any_open())' crates/cladefs/src/cache.rs`

cache::forget, called by bucket::forget for ShredCell and ForgetTenantKeys, is meant to evict every cached extent that no other open store holds, so that what the scope's keys decrypted is no longer in memory (its docs, doc-16, TASK-411.7). It only dooms an extent when Holders::forget returns true, and that happens only when it removes a holder that is still alive and whose cell is in scope. A holder is a Weak<LocalCell>. Once its store has closed, the Weak is dead, and the `None => false` arm drops it without setting `forgot`, because a dead holder's cell can no longer be known. Extents stay cached after their store closes: nothing in open_cell or cladefsd tells the cache when a store closes, and only relief or let_go_least_used remove them later. So in the usual order (the VM stops, then the cell is shredded), none of the cell's cached extents is removed or evicted. Forgotten.extents/bytes report 0, and the cell's plaintext pages stay in the tenant's arena, which the tenant's other VM processes hold read-write, for as long as there is no memory pressure.

**Trigger.** 1) Tenant T's VM opens cell C and reads files. C's extents are cached in T's arena, holders = [Weak(C's LocalCell)]. 2) The VM stops and C's store closes. The holder's Weak is dead, and the extents and their Present pages stay in the cache map. 3) The operator calls ShredCell(C) (or ForgetTenantKeys(T)). bucket::forget -> halt_stores (none open) -> cache::forget(scope). 4) For each of C's extents, Holders::forget drops the dead holder and returns false, so the filter is false. The extent is neither removed nor evicted. 5) C's decrypted pages stay in daemon memory, in T's arena memfd, which T's VM processes map. Shredding promised they were gone.

**Why the tests stay green.** stores_of_shredded_cells_and_tenants_are_halted_and_leave_the_cache (bucket.rs) keeps every store open in its `stores` Vec through both forgets. It never shreds a cell whose store has closed but whose extents are still cached.

**Backlog.** Related to TASK-411.7 (Done), whose shred promised to evict what the scope's keys decrypted but was tested only with stores open, and to TASK-103 (Done), which is why extents outlive their store.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 ShredCell(C) or ForgetTenantKeys(T) issued after C's store has closed removes every cached extent of C that no other open store holds. Forgotten reports those extents, and the extent cache no longer has them.
- [ ] #2 Extents an open store of another cell (a fork) still holds stay in the extent cache.
- [ ] #3 A test in crates/cladefs/src/bucket.rs's test module caches an extent of an encrypted store of tenant 1, closes the store, shreds the cell, and asserts that forgotten.extents == 1 and cache::keys() no longer lists the extent. It goes red (extents == 0) with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
