---
id: TASK-444.14
title: >-
  sqlite_extents: dropping a Registration frees the VFS while a connection with
  no file of it open (an in-memory database) still calls it: use after free
status: Done
assignee: []
created_date: '2026-11-16 14:48'
updated_date: '2026-11-18 03:37'
labels:
  - bug
  - unsafe
dependencies: []
parent_task_id: TASK-444
priority: high
ordinal: 881000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Registration::drop checked only that no file of the volume was open, then unregistered and freed the sqlite3_vfs, its name and its App. A connection opened on the VFS with none of its files open, as an in-memory database's, keeps db->pVfs and calls it after: julianday('now') reads xCurrentTimeInt64 through freed memory. AddressSanitizer: heap-use-after-free in sqlite3OsCurrentTimeInt64 (sqlite3.c:27963). Found by TASK-444.4's audit (both Codex models Incorrect on vfs.rs's Registration). Test: sqlite_extents tests/sqlite.rs a_connection_outlives_its_registration, red under ASan.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 What SQLite was given of a registration stays valid for the process; its volume is held only while the registration or an open file holds it
- [x] #2 a_connection_outlives_its_registration passes under ASan (leaks not checked: the VFS's few hundred bytes stay by design)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
The sqlite3_vfs, its name and its App are the process's (leaked, a few hundred bytes); the App holds the file set weakly, the Registration strongly; opens after the file set went are refused. Verified: tests/sqlite.rs a_connection_outlives_its_registration red under AddressSanitizer before (heap-use-after-free in sqlite3OsCurrentTimeInt64), green after (detect_leaks=0); sqlite_extents tests pass on macOS and ubu1. Pushed 802160e3d, main 558c354e2.
<!-- SECTION:FINAL_SUMMARY:END -->
