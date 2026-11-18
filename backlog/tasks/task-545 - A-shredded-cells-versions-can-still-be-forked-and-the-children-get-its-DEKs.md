---
id: TASK-545
title: 'A shredded cell''s versions can still be forked, and the children get its DEKs'
status: To Do
assignee: []
created_date: '2026-11-18 15:24'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 936000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), crypto review; verified against the code by independent reviewers. Severity: high.

**Where:** `crates/bucket_cell/src/keys.rs`: `rg -n -F 'source.places.values().filter(|&&of| of != child).map(|of| of.tenant).collect();' crates/bucket_cell/src/keys.rs`

After ShredCell, cladefs::bucket::forget(Scope::Cell) halts the cell's open stores, including a version store the daemon keeps open for restores (cells.rs keep/kept, LINGER is 60 s from the last fork, so steady restores keep it open indefinitely). It also adds the cell to CachingKms::shredded. But: (a) OpenCell::fork/fork_to/fork_at_version check only `access().may_manage()`, never `halt.check()`. (b) keys::inheriting refuses only *tenants* in `kms.refuses(..)` and never looks at CachingKms::shredded. (c) finish_fork reads the parent's head (forks_check_versions) and checks only that the version is named and shared. `keys::shred` keeps `versions`, so a shredded head still names the version, and `head.shredded` is ignored. (d) The fork path makes no KMS call for the parent: the DEKs come from the in-memory version ring and the child's KEK from the pool. So each restore after the shred makes a new cell whose head inherits the shredded cell's DEKs rewrapped under a live KEK. The child's ring then holds them (keys::ring `ring.holds(parent)` -> continue) and reads the shredded data for good. Because of (c) this happens even on hosts that were told. doc-16 (TASK-411.7) says takings and holdings of a shredded cell are refused and its KEKs unusable. Forks are a fresh way in that skips both.

**Trigger.** 1) A zygote/snapshot cell C has version v; restores on host H go through cells::fork_version, so H keeps C's version store open (kept). 2) Operator calls ShredCell(C) on H: keys::shred empties keks and sets shredded; forget halts the kept store (EACCES), drops rings and takings, and marks C shredded in CachingKms. 3) Within LINGER of the last restore, a VM process sends ForkOf{C, v} (same tenant, or another tenant if v is shared) -> opened_version finds the dir -> fork_kept -> kept(&key) returns the halted store -> kept.store.fork(&to, tenant) -> host_cell::fork -> inheriting (no shred check; KEK from the pool) -> forked -> finish_fork (head names v, shredded ignored) -> child head with C's DEKs -> make_child -> open_store opens the child and serves C's data.

**Why the tests stay green.** upload::encrypted::shredded_cells_leave_their_cells_unreadable_and_the_tenants_others_readable and cladefs remote::tests::stores_of_shredded_cells_and_tenants_are_fenced_and_leave_the_cache check halted requests, fresh openings and fetchers. No test forks a version (kept or new) of a cell after it is shredded.

**Backlog.** Related to TASK-411.7 (Done), which promised that takings and holdings of a shredded cell are refused and its KEKs unusable but never tested forks, and to TASK-411.4 (Done), whose DEK rewrap on fork carries the shredded keys into the child.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 After ShredCell(C), forking a version of C fails with EACCES and writes no child head to the bucket. This holds whether the version's store is kept open on the host or opened fresh, from C's tenant, and from another tenant when the version is shared.
- [ ] #2 A host that was never told of the shred also refuses the fork, because C's head says it is shredded.
- [ ] #3 A cladefsd test makes a version of C, restores it once so it is kept, shreds C, then restores again from the same tenant and from another tenant. A lifecycle unit test forks a shredded cell's version. Both go red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
