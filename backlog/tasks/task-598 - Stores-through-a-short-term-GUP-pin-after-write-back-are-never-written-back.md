---
id: TASK-598
title: Stores through a short-term GUP pin after write-back are never written back
status: To Do
assignee: []
created_date: '2026-11-18 15:31'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 989000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), guest-dax review; verified against the code by independent reviewers. Severity: medium.

**Where:** `guest/linux/fs/cladefs/dax.c`: `rg -n -F '.dirty_folio = noop_dirty_folio,' guest/linux/fs/cladefs/dax.c`

A page joins dax.c's dirty set only through an iomap: a write fault's mkwrite, a write(2) copy, or zeroing. Write-back swaps the dirty set out and write-protects the PTEs (cladefs_dax_protect), then copies the window page to a new extent. Invariant 3 of doc-13 assumes that afterwards no copy into a clean private page is under way. A short-term FOLL_PIN breaks that assumption, and FOLL_LONGTERM is refused but non-longterm pins on fsdax VMAs are allowed. Examples: FUSE direct_io and NFS O_DIRECT reads, block O_DIRECT reads, and process_vm_writev. Such a writer faults the page dirty when it pins it, then stores through the kernel mapping or by DMA later. That store bypasses the now read-only PTE, so it never faults again. The kernel's own fix-up for this is folio_mark_dirty_lock / set_page_dirty_lock at completion (bio_check_pages_dirty, fuse_release_user_pages), and here it calls cladefs_dax_aops.dirty_folio = noop_dirty_folio. That sets PG_dirty, which dax.c never reads. Neither cladefs_dax_write_back_keeping nor cladefs_dax_rebind_range checks folio_maybe_dma_pinned / dax_layout_busy_page_range (only cladefs_dax_release does). So if a write-back swaps and copies the page between the pin and the late store, the store is in no extent and in no dirty set. A later fsync writes nothing for the page and returns 0. The next rebind (the budget, a freeze, cladefs.dax_rebind_now) MAPs the old extent page over the private page, and the store disappears from the window too.

**Trigger.** 1) A process mmaps DAX file F (MAP_SHARED, PROT_WRITE) at p. 2) Thread 1 runs pread(fd, p, 1 MiB, 0) on a file of an in-guest FUSE filesystem opened with direct_io (FOPEN_DIRECT_IO), or on an NFS file with O_DIRECT. fuse_get_user_pages pins p's pages with FOLL_WRITE, which calls cladefs_dax_mkwrite -> cladefs_dax_dirty: dirty and private. 3) Before the FUSE server replies, thread 2 calls fsync(F), or the flusher or budget worker writes F back: cladefs_dax_write_back swaps the pages out, protects the PTEs and copies the old bytes into Fxfs. 4) The server replies; fuse_copy_page writes the data into the pinned pages through kmap, and fuse_release_user_pages calls folio_mark_dirty_lock -> noop_dirty_folio. 5) pread returns, then the process calls fsync(F): dirty_pages is 0, write-back does nothing, cladefs_fsync commits, and fsync returns 0. A crash then loses the read-in bytes. Without a crash, the next rebind maps the old extent pages back and loads see the old data.

**Why the tests stay green.** guest/sim models a pin as only a reference on the page (sim_pin_user_page, README 'A pin ... is the page's reference alone'), with no deferred store through it. The guest checks' pin cases (window.rs, the vmsplice slice_held/window_held_killed tests) pin pages for reading into a pipe and never store through the pin. The fsx script's O_DIRECT reads target cladefs files, whose direct I/O goes through a bounce buffer and copy_to_iter (PTE stores), not into pinned DAX pages.

**Backlog.** Related: TASK-79.49 (a fault waiting on its own GUP pins) and TASK-149.31 (a store to a slice page a pin holds), both Done. Neither covers a store through a pin after write-back.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A store can land through a short-term FOLL_WRITE pin of a mapped DAX page after a write-back has copied that page, for example a FUSE direct_io or NFS O_DIRECT read into a MAP_SHARED mapping. The next fsync then writes the store to the file. A rebind after it (the budget, cladefs.dax_rebind_now) leaves loads and read(2) returning the store.
- [ ] #2 A guest/sim scenario pins a page with FOLL_WRITE, runs cladefs_dax_write_back, stores through the pin's kernel address and releases the pin with folio_mark_dirty_lock. It fsyncs and asserts the extent holds the store, then rebinds and asserts a load and a read(2) return it. It goes red with the fix reverted. A guest case with a delaying FUSE direct_io passthrough reading into a DAX mmap while another thread fsyncs covers it end to end.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
