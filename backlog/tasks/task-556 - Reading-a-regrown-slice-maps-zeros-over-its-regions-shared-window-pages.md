---
id: TASK-556
title: Reading a regrown slice maps zeros over its region's shared window pages
status: To Do
assignee: []
created_date: '2026-11-18 15:25'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 947000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), guest-dax review; verified against the code by independent reviewers. Severity: high.

**Where:** `guest/linux/fs/cladefs/dax.c`: `rg -n -F 'if (!dax->read_only && !cladefs_dax_pages_mapped(range->state, page)) {' guest/linux/fs/cladefs/dax.c`

A slice's window space is not its own. Its pages are its region's pages, and every file whose single piece is those same extent blocks maps them at the same window address. A reflink copy does this, and so does a file sliced at another offset. When a truncate's new size is page-aligned, nothing drops the slice: cladefs_dax_resize_begin calls zero_part only for a non-aligned size, and zero_part is what would return -EXDEV and drop it. cladefs_dax_resize_end then skips the MAP_ZEROS/UNMAP for a slice ('A slice's pages past the end stay its region's') but still clears those pages in the slice's state with cladefs_dax_pages_unmap. If the file is grown back to the same page count by ftruncate or fallocate, cladefs_dax_fit keeps the slice: `range->pages >= cladefs_dax_range_pages(chunk, size)` holds. The next read of the regrown part finds the page unmapped, and cladefs_dax_iomap_read calls cladefs_dax_map_missing. That calls cladefs_dax_map_pages, which MAPs zeros at cladefs_dax_page_offset(range, page) = region->window_offset + extent page. This is the shared region, and nothing checks range->region. cladefs_dax_iomap_write returns -EXDEV for slices before it calls map_missing, but the read path has no such guard. The pages_fill WARN looks only at private bits, so this happens silently. From then on, every slice of that region at those pages reads zeros: read(2), read faults, and any new slice, since cladefs_dax_region_map finds the region already mapped and sends no MAP. This lasts until the region's last slice goes. The fault entered with the TASK-149.14 seed-1669 fix (map_missing in the read iomap), which assumed only own ranges have unmapped pages.

**Trigger.** 1) In a DAX mount, a.dat is an 8 KiB file packed into a sealed staging extent. 2) `cp --reflink=always a.dat b.dat` (FICLONE; coreutils >= 9 does this by default), then sync. 3) Evict both inodes or remount, then `cat a.dat b.dat`: both become slices of one region at the same window pages. 4) `truncate -s 4096 a.dat`: resize_begin skips zero_part (aligned), and resize_end leaves the slice with page 1 unmapped in its state. 5) `truncate -s 8192 a.dat`: fit keeps the slice (2 >= 2), and Fxfs has a hole at [4096, 8192). 6) `cat a.dat`: iomap_read at pos 4096 -> map_missing -> map_pages -> maps_of yields a zeros map at the region's page -> the device MAPs zeros over the window page that b.dat's slice maps. 7) `cat b.dat` and loads through b.dat's mapping now return 4 KiB of zeros where its data is. Copying b.dat (cp, an editor save) makes the loss durable.

**Why the tests stay green.** guest/sim interleave truncates to 32-byte cells, so page-aligned sizes are rare, and its clone target (the twin) is built from ranges at random offsets. The twin is never a whole-file single-piece slice of the private file's region, so no slice shares the zeroed window pages and the per-file model expects zeros for A anyway. The fsx-style script (guest/tests/test_cases/src/script.rs) picks truncate sizes as random byte counts (`rng.next(MAX)`), and needs both files read in as slices of one extent. hole_mapped_then_written exercises map_missing on an own range only. No test reads a second slice of the same region after a page-aligned shrink-and-regrow of the first.

**Backlog.** RELATED: the TASK-149.14 seed-1669 fix (Done) put map_missing in the read iomap assuming only own ranges have unmapped pages; TASK-160.4, TASK-149.20.4 and TASK-149.47 (Done) cover page state and clone-shared regions but not a slice regrown after an aligned shrink.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 After a slice is shrunk to a page-aligned size and grown back to its page count by truncate or fallocate, reading its regrown pages returns zeros without any MAP over its region's window pages: another slice of the same region (a reflink copy of the file) still reads its own bytes through read(2), through a read fault and through any new slice.
- [ ] #2 A regression test in guest/sim (tests/slice_regrown_shares_zeros.c) writes a 2-page file A, fsyncs, clones A whole onto B, reads both in as slices of one region (window_slices == 2, same window_offset), truncates A to one page then two, reads A's page 1 expecting zeros, then reads and loads B's page 1 expecting its original bytes; a guest check does the same with cp --reflink=always, truncate and cmp; both go red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
