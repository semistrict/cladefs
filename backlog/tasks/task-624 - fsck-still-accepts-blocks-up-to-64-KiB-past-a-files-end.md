---
id: TASK-624
title: fsck still accepts blocks up to 64 KiB past a file's end
status: To Do
assignee: []
created_date: '2026-11-18 15:35'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 1015000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), fxfs-unreviewed-delta review; verified against the code by independent reviewers. Severity: low.

**Where:** `crates/fxfs/src/fsck/store_scanner.rs`: `rg -n -F '&& range.end > max_extent_end(bs, attribute.size)' crates/fxfs/src/fsck/store_scanner.rs`

store_scanner flags ExtentExceedsLength only past `max_extent_end`, which rounds the size up to the next 64 KiB (`APPEND_RESERVATION`, line 38). That allowance was added in TASK-133 for writable DAX's `fxfs_dax_append_end` reservations, but commit 37c331a6c (2026-11-04, 'writable DAX deleted') removed the code that made them. Nothing in the tree reserves blocks past EOF now, and the guest check test_append_reserved_tail (appends.rs) now asserts that no blocks lie past the end. fsck's own test_blocks_reserved_past_the_end (fsck/tests.rs:2359) still checks that such a state passes. This matters because only `grow` (truncate up) trims extents past the old end (`trim_some(TrimMode::FromOffset(old_size))`). The other growth paths, `txn_grow_size` (txn_write, write_many), `clone_range_in_commits`'s size update and `zero_tail_before_growth`, only zero the last block and then raise the size. Any extent left between align_up(size) and the next 64 KiB boundary becomes file data again when the file grows past it by a write, writeback or clone: stale bytes where reads should return zeros, on read(2) and through extents_in/DAX MAP too. fsck runs after every Fxfs test and on the host's image checks, and it would not flag a regression that leaves such blocks.

**Trigger.** Example of the hidden failure: a 5000-byte file keeps an extent at 8192..12288, for example from a truncate whose trim commit failed (truncate_with_options logs 'Failed to trim after truncate' and returns Ok), or from any future bug that writes a block past EOF. fsck passes because 12288 <= 65536. A later write_many at 16384 grows the size to 20480 through txn_grow_size, and reads of 8192..12288 return the stale block instead of zeros. With the upstream bound (align_up(size)), fsck would have reported ExtentExceedsLength at the first check.

**Why the tests stay green.** test_blocks_reserved_past_the_end requires fsck to accept exactly this state, and no other fsck test places an extent between align_up(size) and the next 64 KiB boundary.

**Backlog.** No related task covers it: TASK-133 (Done) added the 64 KiB allowance for writable DAX's append reservation; TASK-164.7 (Done) deleted writable DAX without removing it.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 fsck reports ExtentExceedsLength for an extent past align_up(size) of a file not in the graveyard: a 123-byte file with a block at 60 KiB fails fsck.
- [ ] #2 test_blocks_reserved_past_the_end in fsck/tests.rs is inverted to expect ExtentExceedsLength, and goes red with the 64 KiB allowance put back.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
