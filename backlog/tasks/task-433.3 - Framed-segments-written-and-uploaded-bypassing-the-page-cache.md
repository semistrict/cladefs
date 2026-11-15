---
id: TASK-433.3
title: Framed segments written and uploaded bypassing the page cache
status: Done
assignee: []
created_date: '2026-11-15 10:27'
updated_date: '2026-11-15 14:02'
labels: []
dependencies: []
parent_task_id: TASK-433
ordinal: 826000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
A framing writes its file through the page cache, written back a piece at a time, and the uploader reads it back from there (TASK-401's read_object). Write it with O_DIRECT; the upload reads it from disk, or from what the framing holds in memory.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done by one page cache of the store's own (extent_store::pages, after Linux's mm/filemap.c, page-writeback.c, fs-writeback.c): HostDir's extents' files O_DIRECT/F_NOCACHE, through it; governed by extentd's governor, AnonBudget.pages. Bugs found on the way: (1) DiskFile::size raced direct::write_at's whole-block write before set_len (uploader read 4096 for 592 bytes, a truncated object): size takes the tail lock; blind spot: no test read size while a write lands, the flaky upload test (0/10 after) is the test of the class. (2) the daemon's disk count walked the disk while writes sat dirty in the cache: recount writes back first, after reading what is being written; caught by tests/disk.rs and hostile.rs's disk limits on ubu1. (3) opens with data=false of a cached path bypassed the cache (EIO): every open of a registered path shares its pages. Structure: one owner of a file's pages per path (HostDir's registry), so no two openings hold different bytes. Follow-up: TASK-433.5 (crash rounds over the cache). Verified: macOS workspace 2495/2495 and clippy gate after the cladefs merge; ubu1 workspace 2795/2795 (fxjob t433b); upstream-changes.py --check passes. Pushed d2cfd3b4c.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Store data files bypass the host's page cache, held instead in extent_store::pages under the governor's one budget; macOS 2495/2495, ubu1 2795/2795; pushed d2cfd3b4c.
<!-- SECTION:FINAL_SUMMARY:END -->
