---
id: TASK-592
title: A seal-refused reserved writeback pays twice for its blocks and can drop data
status: To Do
assignee: []
created_date: '2026-11-18 15:30'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 983000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), fxfs-alloc review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/fxfs/src/object_store/data_object_handle/write_many.rs`: `rg -n -F 'if packed && error.chain().any(|cause| cause.is::<SealedExtent>()) =>' crates/fxfs/src/object_store/data_object_handle/write_many.rs`

When the device refuses a packed write with SealedExtent, `write_many_paid` drops the transaction and retries unpacked. Dropping it runs `Allocator::drop_mutation`. Because S is still pinned or Allocated, `ExtentTable::drop_allocation` returns `kept` = the dropped range (TASK-211: the staging extent keeps the range), and `reservation.commit(kept)` (allocator.rs:2307) takes those bytes out of `taken.reservation`. That reservation is what `DirtyReservation::take` gave the writeback: cost(bytes) + SPARE, sized to write each block once. The unpacked retry needs the same bytes again. `reserved_transaction_options` tries to top up from the allocator. On a filesystem filled by other files' dirty reservations that fails, and it borrows metadata only. For a packed run of about 30–64 blocks (120–256 KiB), what is left (SPARE plus about 768 bytes per block) is short. `allocate_in` returns len 0, giving NoSpace. This breaks TASK-80's promise that reserved dirty data is always written.

**Trigger.** Guest page-cache writeback: `cladefs_vfs_write_run` → `cladefs_write_many` → fxfs-posix `write_dirty` → `DirtyReservation::take` → `write_many_reserved`. The filesystem is full of other files' reserved dirty data. File F's run of about 200 KiB packs into S, after another file's transaction T1, which allocated the range below it. T1 writes S and a sync barrier seals S. F's write is refused (SealedExtent), and F's transaction drops with kept = 200 KiB committed out of `taken`. The retry's allocation gets only about the 96 KiB SPARE plus the per-block share, and the next allocation fails NoSpace. `write_dirty` returns ENOSPC. `cladefs_vfs_write_run` calls `mapping_set_error`, the pages are clean, and F's data is lost even though its write(2) had reserved room.

**Why the tests stay green.** `test_write_many_tries_again_after_a_seal` injects SealedExtent only into plain `write_many` on a filesystem that is not full. The dirty_reservation tests (`fill_and_write_back`, `test_dirty_data_with_no_room_is_owed`) fill the filesystem but never seal the staging extent between allocation and write. No test combines a reservation, a full filesystem and a seal race.

**Backlog.** REGRESSION of TASK-80 (Done), whose AC #3 promised writeback never drops dirty data because the filesystem filled after the write; 37c331a6c added the packed-then-unpacked retry and TASK-211's 095119138 (Done) takes kept bytes out of the transaction's reservation.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 On a filesystem filled by other files' dirty reservations, a reserved writeback whose packed write is refused with SealedExtent and retried unpacked succeeds for runs up to STAGED_MAX (64 blocks), and the data reads back.
- [ ] #2 The guest's page cache never drops dirty data whose write(2) reserved room: write_dirty returns no ENOSPC after a seal refusal.
- [ ] #3 A regression test in fxfs's dirty_reservation.rs fills as fill_and_write_back does, writes back one 48-block run with write_many_reserved while a FakeDevice callback refuses the first data write with SealedExtent after an earlier committed allocation in the staging extent, expects success and the contents back, and goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
