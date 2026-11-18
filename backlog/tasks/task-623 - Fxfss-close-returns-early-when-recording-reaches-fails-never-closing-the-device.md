---
id: TASK-623
title: >-
  Fxfs's close returns early when recording reaches fails, never closing the
  device
status: To Do
assignee: []
created_date: '2026-11-18 15:35'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 1014000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), fxfs-unreviewed-delta review; verified against the code by independent reviewers. Severity: low.

**Where:** `crates/fxfs/src/filesystem.rs`: `rg -n -F 'self.allocator().record_reaches().await?;' crates/fxfs/src/filesystem.rs`

close() sets `closed`, waits for the graveyard and cancels background tasks. It then calls `record_reaches().await?`. If that call fails, close returns straight away and skips everything after it: `journal.stop_compactions()`, the final `journal.sync(flush_device)`, `allocator().freed()`, `journal.terminate()`, joining the flush task, and `device().close()`. The comment at the end of the same function says the device must close whether or not sync succeeds. A sync failure does take that path, but a reach failure does not. On the guest, `device().close()` is the final COMMIT, which seals and is uploaded. Without it, journal records committed since the last flush are never sealed, and the store recovers to the previous commit. `closed` also stays true, so any second close hits `assert_eq!(self.closed.swap(true...), false)`. There is a related defect: `record_reaches` puts the drained reaches back only when `new_transaction` fails. When `transaction.commit()` fails (allocator.rs:1665), they are lost, and the next mount sizes the extent short again (the TASK-213 problem comes back after a failed commit).

**Trigger.** 1) A transaction holding an allocation in the staging extent is dropped after its bytes reach the device. Examples: write_many_paid's SealedExtent retry drops the packed transaction, or a writeback fails between its bytes and its commit. This leaves an entry in `unrecorded_reaches`. 2) The guest unmounts: kill_sb -> cladefs_close -> Filesystem::close -> FxFilesystem::close. 3) The reach transaction's commit fails, for example Journal::pre_commit/make_room failing to extend the journal on a full device, or a device error surfacing there. 4) close returns Err at line 714. cladefs_close only logs it. The device's closing COMMIT (uploaded) is never sent, the journal is not synced, and the flush task is not terminated. Transactions committed since the last sync are lost even though the journal and device could still have taken a plain sync and commit.

**Why the tests stay green.** No test makes record_reaches fail. Every test that closes has either no pending reaches or a commit that succeeds (allocator.rs test_a_shared_extent_takes_what_was_dropped_in_it_after_a_remount closes successfully). Guest cases never inject a failure into the reach transaction at unmount.

**Backlog.** Related. TASK-213 (Done) added the record_reaches call in close without handling its failure; TASK-233 (Done) is a commit-lock deadlock, a different defect.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 When record_reaches fails at close (its commit failing in pre_commit), close still writes the journal out, ends the flush task and calls device().close(), so the device's closing commit is sent; close still returns the error.
- [ ] #2 After that failed commit the reach is still pending in unrecorded_reaches.
- [ ] #3 A filesystem test with a pending staging-extent reach and Hooks::on_pre_commit failing its commit asserts FakeDevice saw close and the journal written; it goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
