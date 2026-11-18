---
id: TASK-625
title: An O_DIRECT write past EOF turns what a mapping stored past EOF into file data
status: To Do
assignee: []
created_date: '2026-11-18 15:35'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 1016000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), guest-vfs review; verified against the code by independent reviewers. Severity: low.

**Where:** `guest/linux/fs/cladefs/super.c`: `rg -n -F 'err = cladefs_nofs(cladefs_write(fs, inode->i_ino, pos + done, buf, len));' guest/linux/fs/cladefs/super.c`

Every other way a page-cache file grows first zeroes what the cached folio at the old EOF holds past that end: write_begin calls cladefs_vfs_zero_past_end, cladefs_data_resize calls cache_resize_begin, and remap calls cache_remap_begin (TASK-79.53, TASK-153.4). The write branch of cladefs_vfs_direct_IO writes straight to Fxfs. generic_file_direct_write then calls i_size_write() with the new end. kiocb_invalidate_pages writes back and invalidates only the write's own page range. So a cached EOF folio outside that range keeps whatever a MAP_SHARED store put past the old EOF. Once i_size covers those bytes, buffered reads return them where they should return zeros. The next writeback of that folio writes them into Fxfs, because cladefs_vfs_writepages writes len = min(folio_size, i_size - pos). This applies to every page-cache inode: all regular files on a non-DAX mount, and on a DAX mount, files created in this mount.

**Trigger.** 1) Write 100 bytes to a new file f. 2) mmap f MAP_SHARED for one page, store 'X' at offset 1000 (past EOF but inside the last page, so no SIGBUS), then msync. Writeback writes 100 bytes and folio 0 stays cached with 'X'. 3) Open f with O_DIRECT and pwrite 8 aligned bytes at offset 8192. kiocb_invalidate_pages touches only page 2, cladefs_vfs_direct_IO calls cladefs_write, and the VFS sets i_size to 8200 without zeroing folio 0. 4) A buffered pread of bytes 0 to 8200 returns 'X' at offset 1000. Any later write to bytes 0-99, or another store, dirties folio 0, and writeback writes all 4096 bytes. 'X' is then in Fxfs and survives eviction.

**Why the tests stay green.** test_store_past_end, test_store_past_end_then_written and test_store_past_end_then_cloned (copies.rs) cover set_len, a buffered write past the end and a clone. The growing write in test_direct_io (files.rs) starts inside the EOF page, so kiocb_invalidate_pages drops that folio. fsx grows a file with ftruncate before it does mapped writes, and never stores past EOF.

**Backlog.** Regresses TASK-79.53 (Done), which promised that bytes an mmap stores past EOF are zeroed before the file grows over them. Its fix covered resize and write_begin but missed the O_DIRECT growing write that TASK-26.19.11 (Done) had already added. TASK-153.4 fixed the clone path the same way.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 After a MAP_SHARED store past EOF, an O_DIRECT write that grows the file from beyond the EOF page leaves zeros from the old end up to the write. Buffered reads return zeros there at once, and so does Fxfs after a buffered write to byte 0 and an eviction.
- [ ] #2 test_store_past_end_then_direct_written in guest/tests/test_cases/src/test_fxfs_guest_checks/guest/copies.rs runs in the All suites (page cache and DAX). It is test_store_past_end_then_written with the growing write replaced by an O_DIRECT pwrite at 2*PAGE, and it goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
