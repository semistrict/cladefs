---
id: TASK-544
title: A refused cross-tenant fork leaves the parent's DEKs in the bucket for good
status: To Do
assignee: []
created_date: '2026-11-18 15:24'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 935000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), crypto review; verified against the code by independent reviewers. Severity: high.

**Where:** `crates/bucket_cell/src/lifecycle.rs`: `rg -n -F '(head.deks, head.inherited) = given.deks.unwrap_or_default();' crates/bucket_cell/src/lifecycle.rs`

fork_version_keyed creates the child's head (Start::Create, line 239) with the keys `Inheriting::given()` built: a KEK from the child tenant's pool, a new DEK, and `inherit(&self.source, ...)`, which is every DEK the version store's ring holds (the parent cell's own DEKs at every generation, plus what it inherited), rewrapped under the child's cell KEK. Only after that does finish_fork check `crosses && !named.is_some_and(|named| named.shared)` (line 339) and return NotShared. host_cell::fork then calls `unfork`, and `lifecycle::delete_fork` -> `ended(found, holder)` swaps the head to FINAL but keeps keks, deks and inherited. The head stays Made::Forking, which `Head::removable()` rejects (it accepts only Root or Clone), so the collector never removes it. Tenant A's DEKs for a version A never shared now sit in tenant B's key hierarchy: B's tenant KEK -> B's cell KEK (in that head's keks) -> A's DEKs (in its inherited list). After A is shredded (its KEK destroyed, or ShredCell), A's objects can still be decrypted by anything holding B's KEK. That breaks doc-16 Goal 2. Under BYOK it also hands A's data to whoever holds B's KMS key and can read the bucket, which breaks Goal 3. keys::inheriting refuses only tenants the host heard were revoked. It does no sharing check, so the rewrap happens before authorization.

**Trigger.** 1) Tenant 1 makes a VM snapshot (a version of cell C1) and does not share it. 2) A VM process on tenant 2's granted socket sends OPEN ForkOf{tenant:1, cell:C1, version:v}. Cell IDs come from sequential allocator blocks and versions are small integers, so they can be guessed. A VM manager restoring into the wrong tenant does the same thing. 3) server.rs open_held -> cells::fork_version -> opened_version opens a version store of C1 here (unwrapping C1's KEK) -> fork_kept -> OpenCell::fork(tenant 2) -> may_fork_into passes (store at a version) -> host_cell::fork -> uploader.inheriting(child): source = C1's version ring, KEK from tenant 2's pool -> uploader.forked -> fork_version_keyed creates tenants/2/cells/<child>/head with inherited = C1's DEKs wrapped under tenant 2's cell KEK -> finish_fork returns NotShared -> EACCES -> unfork leaves the FINAL head with all keys. 4) Tenant 1 is shredded later: C1's segments are still in the bucket and decrypt through tenant 2's chain.

**Why the tests stay green.** grants.rs tenants_restore_no_unshared_version_of_another and tenants_restore_shared_versions_of_others_into_cells_of_their_own check only EACCES and that no local store exists at the path (`!vm.exists()`). Nothing reads the bucket for the child's head or its inherited DEKs. The lifecycle unit test `restore(celled(2, 5), 0xa) == Err(NotShared)` forks with no keys.

**Backlog.** RELATED: TASK-515 (In Progress) moved the keks and inherited DEKs into the child head's create, ahead of finish_fork's sharing check; TASK-395.7 and TASK-411.4 (Done) promise a refused fork makes nothing but never fixed this ordering.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Forking another tenant's unshared version fails EACCES before any of the parent's DEKs are rewrapped: afterwards no head under tenants/<child tenant>/cells holds the parent's DEKs in its inherited list, so shredding the parent's tenant leaves its segments unreadable through the child tenant's KEK.
- [ ] #2 A refused fork returns no cell KEK to the child tenant's pool that was already written into a head in the bucket: the tenant's next new cell gets a cell KEK no leftover head holds.
- [ ] #3 A regression test in cladefsd's grants.rs (or local_cell upload::encrypted) forks an unshared version of tenant 1's cell into tenant 2, asserts EACCES, lists tenant 2's heads in the bucket and finds none inheriting tenant 1's DEKs or sharing a KEK with tenant 2's next cell, and goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
