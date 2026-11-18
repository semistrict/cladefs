---
id: TASK-591
title: A small write waiting out a volume snapshot terminates the journal
status: To Do
assignee: []
created_date: '2026-11-18 15:30'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 982000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), fxfs-alloc review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/fxfs/src/object_store/allocator.rs`: `rg -n -F 'let records = inner.commit(&device_range, data_owner, context)?;' crates/fxfs/src/object_store/allocator.rs`

Allocate's apply records an allocation under `data_owner(owner_object_id)` as it is at apply time, and the mutation carries the store's id. A packed write (`write_many` → `multi_write_packed` → `allocate_shared` → `ExtentTable::stage`) takes blocks of the staging extent S, which belongs to the source's data owner D1 at allocation time. `snapshot_volume` holds `LockKey::commits(source)` while it commits the Snapshot mutation, which moves the source to a new owner D2. A transaction built before that waits at the gate and applies after it. Its Allocate then calls `ExtentTable::commit(range, D2)`. S is Allocated under D1, so `ensure!(owner == owner_object_id, ..)` (extent_table.rs:484) fails with Inconsistent. `apply_transaction` returns the error and the journal calls `terminate("Failed to apply a transaction")`. The filesystem is halted: every later operation fails until a remount, and the write in flight is lost. TASK-241's fix skips frozen extents for Deallocate and Reference only, not for Allocate into a shared extent.

**Trigger.** cladefs::Image API (crates/cladefs/src/image.rs `snapshot_volume`, documented as working on a mounted volume while it is written): (1) a mounted volume is being written with small files through `Filesystem::write` → fxfs-posix `write` → `DataObjectHandle::write_many`, so the staging extent S already has committed allocations under D1. (2) Writer A builds its transaction: `allocate_shared` pins S for a range and the data is written. (3) `snapshot_volume` takes the source's Commits write lock, flushes, and commits Snapshot, so `data_owners[source]` becomes D2. (4) A's commit takes the Commits read lock and applies Allocate{S[..], source}. `data_owner(source)` is D2 and S's owner is D1, so the result is Inconsistent and the journal terminates (journal.rs, `Failed to apply a transaction`).

**Why the tests stay green.** test_snapshot_volume_racing_writes races snapshots with `write_or_append` (`txn_write` → `multi_write`, unpacked), which mints a fresh extent with no owner, so the owner check never fires. fxfs-posix's model test runs Op::Snapshot between operations, never while a packed transaction is in flight. After a snapshot, the next `stage` sees an owner mismatch and replaces S, so sequential tests never commit into the old S.

**Backlog.** Related. TASK-241 (Done) skips frozen extents for Deallocate and Reference only, not Allocate into a staging extent; TASK-246 (To Do) wrongly assumes Allocate names the owner current when built and predicts a leak, which for a staging extent is this termination; TASK-26.16 (Done) rotates staging only for new allocations.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A write_many of a small file held at the source's commit gate while snapshot_volume commits its Snapshot then commits: the journal does not terminate, fsck passes, and after a remount the file reads back.
- [ ] #2 A test in volume/snapshot.rs writes small files with write_many so the staging extent is allocated, holds a further write_many at the gate (LockKey::commits(source) or a failpoint), runs snapshot_volume and releases the write; it goes red with 'Failed to apply a transaction' when the fix is reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
