---
id: TASK-594
title: A journal extent off a 4 KiB boundary panics the guest kernel at mount
status: To Do
assignee: []
created_date: '2026-11-18 15:30'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 985000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), fxfs-journal review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/fxfs/src/object_store/journal/bootstrap_handle.rs`: `rg -n -F 'assert!((len - rest.len()) % self.device.block_size() as usize == 0);' crates/fxfs/src/object_store/journal/bootstrap_handle.rs`

`read_transactions` snoops each committed transaction's root-parent extent mutations for the journal file. It truncates (`truncate_extents(extent.start)`) and pushes (`push_extent(journal_extent(..))`) the replay reader's BootstrapObjectHandle before any mutation is validated: `validate_mutation` runs only after `read_transactions` returns, in both `replay` and `read_on`. Extent keys deserialize when 512-aligned (`SerdeExtent`), but the guest device's block size is 4096 (doc-4). A key starting at T, 512-aligned but not 4096-aligned, below the handle's end, leaves the previous extent with an unaligned length and appends the new extent at T. The next read window that crosses T, in `BootstrapObjectHandle::read_aligned`, reads T - read_offset bytes from the truncated extent and then reaches the pushed one, where `assert!((len - rest.len()) % self.device.block_size() as usize == 0)` fails. linux-rt's panic handler panics the guest kernel. `ObjectValue::is_valid_for` / `ExtentValue::fits` would have refused the key (off blocks), but only after the panic. `SuperBlockRecord::Extent` ranges reach the same assert unchecked through `RecordReader::next_item`'s `push_extent`; that path needs two such records. This is the class TASK-149.9 set out to remove (a flipped byte, resealed, must fail the mount, never panic; its AC #2 is still unchecked). That task fixed a start before the extents in this file but not this assert.

**Trigger.** 1. An image takes a tenant's disk after a bad upload, or a crafted shared version that other tenants fork. After the super-block's offset, its journal holds a committed transaction with a root-parent ObjectStore mutation on the journal object's DATA attribute: Extent key start T = end_offset - 3584 (512-aligned), value ExtentValue::Some. The block checksums are resealed, as corrupt_tests reseals.
2. Mount -> Journal::replay -> read_transactions -> JournalRecord::Commit. The snoop calls reader.truncate_extents(T): the extent holding T now ends unaligned, and ReadAhead keeps a partial block. push_extent then appends the record's extent at T.
3. read_block's next() sees a partial block, so read_aligned(read_offset, window) runs. The truncated extent yields T - read_offset bytes (not a multiple of 4096), and at the pushed extent the assert fails. The guest kernel panics on mount, every time.

**Why the tests stay green.** corrupt_tests flips random bytes. It would need a 512-aligned, not 4096-aligned journal-extent key below the handle's end inside a committed root-parent mutation, which flips almost never make. fuzz_journal's arbitrary records would also have to hit the journal object's ID, AttributeId::DATA and ExtentValue::Some. The bootstrap_handle tests use aligned extents only.

**Backlog.** Related: TASK-149.9 (Done, AC #2 unchecked) requires a corrupt image to fail its mount and never panic. It fixed the neighbouring asserts but not this one, which dates from the upstream import. TASK-92.18 (Done) rewrote the read loop around it.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Mounting fails with FxfsError::Inconsistent, and the guest kernel does not panic, when the image's journal holds a committed root-parent extent of the journal file that starts 512-aligned but not 4096-aligned (e.g. end - 3584). The same holds for two journal extents whose first length is 4608, and for a super-block whose first Extent record is 0..4608 followed by another. The host library's Filesystem::mount and Storage::check refuse these images too.
- [ ] #2 A regression test pinned in journal.rs or corrupt_tests writes such a resealed transaction, and such a super-block, into a mounted image. It asserts FxFilesystem::open returns Inconsistent, and it goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
