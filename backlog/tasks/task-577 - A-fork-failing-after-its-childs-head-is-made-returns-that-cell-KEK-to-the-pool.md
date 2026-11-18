---
id: TASK-577
title: >-
  A fork failing after its child's head is made returns that cell KEK to the
  pool
status: To Do
assignee: []
created_date: '2026-11-18 15:28'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 968000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), crypto review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/bucket_cell/src/keys.rs`: `rg -n -F 'self.kms.give_back(self.child.tenant, (secret, kek.wrapped));' crates/bucket_cell/src/keys.rs`

Uploader::forked takes `inheriting` by value. On any Err from fork_version_keyed (`Err(err) => return Err(err.into())`, upload.rs:609) the Inheriting is dropped, and Drop puts its KEK (secret and wrapped bytes) back into the tenant's pool. But fork_version_keyed has already created the child's head with that wrapped KEK whenever the error comes from finish_fork: NotShared, Deleted (the version dropped between opening the version store and the fork), or a checkpoint_at failure. unfork's delete keeps keks in the FINAL Forking head, and that head is never removed (not `removable()`). The pool then hands the same (secret, wrapped) to the next cell this host makes for the tenant (keys::make, keys::taking or another fork). Two cells now share one cell KEK. ShredCell of the later cell removes its wrapped KEK from its head but leaves its DEKs there. The identical wrapped KEK is still in the dead fork's head, so anyone with the tenant's KMS access unwraps it, derives wrapping_key(kek, cell, gen) and unwraps the shredded cell's DEKs.

**Trigger.** 1) Host H's pool for tenant T holds KEK K (wrapped W). 2) A restore into a T cell races DropVersions of its version (or is a cross-tenant restore refused NotShared): fork_version_keyed creates the child head with keks=[W], then finish_fork returns Deleted/NotShared -> forked() returns Err -> Inheriting dropped -> give_back(T, (K, W)) -> unfork ends the head, still holding W. 3) H next makes a cell X of T (CreateCell -> keys::make pops K): X's head has keks=[W]. 4) ShredCell(X): X's keks are emptied, but W still sits in the dead child's head -> unwrap W via T's KMS -> K -> X's DEKs unwrap -> X readable after its shred.

**Why the tests stay green.** Fork tests check the pool count after successful forks (forks_into_other_cells_keep_what_they_inherit_rewrapped_...) and that a refused fork makes nothing locally. No test fails a fork after the head create and then compares the pool's next KEK against the bucket's heads.

**Backlog.** Related to TASK-515 (In Progress), which moved the keys into the fork's head create, so the KEK is used before the fork can fail, and to TASK-411.7 (Done), whose promise that a shredded cell is unreadable this defeats.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A fork that fails after its child's head was created (finish_fork returning NotShared or Deleted, or a checkpoint_at failure) never gives that head's cell KEK back to the tenant's pool. Neither does a fork that a resume finished first. The next cell the host makes for the tenant gets a different wrapped KEK.
- [ ] #2 After such a failed fork, ShredCell of any cell made later leaves that cell's DEKs unwrappable: no other head in the bucket holds its wrapped KEK.
- [ ] #3 A test in bucket_cell's keys or upload::encrypted tests uses a pooled CachingKms and forks into a child whose version is dropped (or unshared) after the head create. It asserts that the fork fails, that the pool shrank by one, and that the next new_cell_kek's wrapped bytes differ from the dead head's. It goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
