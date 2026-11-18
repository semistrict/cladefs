---
id: TASK-540
title: >-
  Closing a file set frees the dying list its root names, so it never opens
  again
status: Done
assignee:
  - '@claude'
created_date: '2026-11-18 15:23'
updated_date: '2026-11-18 22:06'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 931000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), file-sets review; verified against the code by independent reviewers. Severity: critical.

**Where:** `crates/extent_files/src/file_set.rs`: `rg -n -F 'inner.dying = block_on(free(&**self.device, &dying));' crates/extent_files/src/file_set.rs`

When a commit leaves more than FREE_BATCH (64) extents dying, commit_root mints a list extent L, writes it and commits a root whose `dying_list` is (L, n). It then frees the listed extents and puts L into `inner.dying` (`inner.dying.extend(list);`), meaning L should go after the next root that no longer names it. `close()` frees everything in `inner.dying`, L included, with no commit after it. Its comment assumes everything there is listed as dying in the last root, but L is named there as the list that recovery must READ. `recover` then runs `device.read(list, 0, ..)` with `.context("reading the dying extents' list")?`. The store no longer holds L: OpenCell::read -> read_cached sees `store.extent(L)` is None -> read_stored -> NoExtent. So the opening fails, and every later one fails too. A second way in: after a crash, `recover` itself appends `root.dying_list`'s L to `inner.dying`, so an open followed by a close with no commit in between does the same. This hits both SQLite cells and memory cells (MemoryCell::close calls files.close()).

**Trigger.** 1) A SQLite cell through sqlite_extents (daemon::open or FileSet::open), with a database built over >= 65 transactions that each leave an extent at least half live (e.g. 65 batches of 1000 INSERTs of new rows). 2) One commit leaves >= 65 extents unreferenced: VACUUM, an UPDATE that rewrites every row, a truncation, or FileSet::delete of that database. commit_root sees dying.len() > 64, writes list extent L, commits root{dying_list:(L,n)}, frees the n extents, and sets inner.dying = [L]. 3) The application shuts down cleanly: sqlite_extents::FileSet::close -> extent_files::FileSet::close frees L, then device.close() (COMMIT_UPLOADED). 4) Reopen: FileSet::open -> recover -> Root::decode gives dying_list Some(L) -> device.read(L) fails with NoExtent -> open fails with "reading the dying extents' list". No commit can ever replace the root, because committing needs an opened file set, so every database in the cell stays unreadable through it. A memory cell does the same: a snapshot commit kills >64 old 4 MiB extents, then MemoryCell::close, then MemoryCell::open.

**Why the tests stay green.** tests/crash.rs and the sqlite tests reopen crash images or device snapshots and never call close() first. The daemon tests (sqlite_extents/tests/daemon.rs Mounted::close, the cladefsd memory-cell tests) do close and reopen, but the last commit before each close is a single-row update or a small write that leaves far fewer than 65 extents dying, so no root naming a list is ever followed by close(). Nothing checks that close() leaves every extent the last root names readable.

**Backlog.** No related task covers it: TASK-296 and TASK-301 (Done) built the file set and its dying list without claiming anything about close; TASK-516 and TASK-530 (To Do) are other defects in file_set.rs.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 After a commit whose root names a dying list (65 or more extents left dying by VACUUM, a full-row UPDATE, a truncation or a delete), a clean close followed by an open succeeds and reads every file as last committed, for a SQLite cell (sqlite_extents::FileSet::close) and for a memory cell (MemoryCell::close then MemoryCell::open).
- [x] #2 A file set opened after a crash whose root names a dying list, then closed with no commit in between, opens again.
- [x] #3 A regression test in extent_files on a FakeDevice writes a file in 70 commits to fresh chunks (small max_extent_blocks), truncates it to 0 and syncs so the root names a dying list, closes, and reopens from the device's image expecting success and an empty file; a second case reopens after the listing commit, closes with no commit and reopens again; both go red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Keep the list extent the last root names out of inner.dying: a field of its own (Inner::list), set by recover and commit_root, moved among the next commit's dying, restored on a failed write, counted by extents(); close() frees only inner.dying.
2. Prove: extent_files model (both fixed cases: close after a listing commit; crash then close with no commit), sqlite rounds closed after each, and a memory-cell close-and-reopen test in cladefsd tests on ubu1.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Blind spot: no test of extent_files, sqlite_extents or memory cells called FileSet::close() and then opened the file set again. They reopened crash images or device.snapshot() of a file set still open, which never runs close. a_commit_freeing_many_extents_lists_them does reach the dying list, but only through crash images, and its last commit is a one-row insert, which stops naming the list. extent_files had no randomized test at all; the only model test with a close-and-mount operation (cladefs storage/model_tests.rs) covers Fxfs images, not file sets.

Tests added (red until this is fixed, each failing with "reading the dying extents' list"):
- crates/extent_files/tests/model.rs: a random model of a file set, on a device and in a window. Operations: write, rewrite, punch, truncate, sync, delete, open and close a file, fetch and give back a page, close and reopen, crash and reopen. 48 seeds of 300 operations. It checks every file against the model and that the device holds exactly what the file set names. It also holds two fixed cases of this bug, plus a_fetched_page_reads_as_it_was_until_given_back.
- crates/sqlite_extents/tests/sqlite.rs a_database_opens_as_it_was_closed_after_every_kind_of_transaction: 12 rounds, each of 100 to 250 small transactions and then a DELETE, VACUUM or full UPDATE. After each round the file set is closed, reopened, and checked for integrity and rows. A temporary print showed SQLite only fails if the freeing transaction is the last commit before the close.

Checked with the code temporarily changed:
- With close() not freeing, the model passes in both modes, so it finds nothing else at this layer.
- With refresh remapping while pages are fetched, the held-page case fails and the random runs do not: random fetches rarely land on a clean, mapped page (holes can't be fetched), so the fixed case carries that check.

Not covered: memory cells (crates/cladefs/src/memory_cell.rs is Linux-only and needs the daemon) have no close-then-reopen test yet. FakeDevice::close only sets a flag, while DaemonDevice and ImageDevice commit on close, so crash images never include what close freed.

Fixed: Inner::list holds the list extent the last root names, apart from inner.dying; close() frees only inner.dying; the next commit counts the list among its dying (restored on a failed write); extents() names it. Verified: macOS scripts/test.sh clippy+workspace, 2622 passed. ubu1 (fxjob): extent_files, sqlite_extents and cladefs memory tests, 68 passed. New cladefsd test a_memory_cell_opens_again_after_a_commit_freeing_many_extents fails with 'reading the dying extents' list' on the old file_set.rs and passes with the fix. The extent_files model and the sqlite test were red before the fix and are green after it. upstream-changes.py --check passes; extent_files is cladefs's own code, so no upstream deviation. Linux full workspace: left to CI on main.

Linux: CI run 44384073802 (bef96fd9f) test (workspace) passed the new tests: a_memory_cell_opens_again_after_a_commit_freeing_many_extents, the extent_files model, and the SQLite close/reopen rounds. Its 4 failures are the cladefsd EBUSY race filed as TASK-644, failing on main before this change too. The model then took 210 s on CI and 138 s on ubu1, against 5.5 s on macOS. perf showed 80% in BufferSource::new's alloc_zeroed: a page-aligned layout can't use calloc, so Linux zeroed all 64 MiB of each FakeDevice made at every snapshot. Fixed in 97f0dead8 (host BufferSource is anonymous mmap, guest keeps alloc_zeroed) and 1ab... (CopyingWindow sized by its test): the model now takes 5.4 s on ubu1, and the memory-cell tests are no slower (A/B, three rounds).
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
FileSet::close no longer frees the dying list the last root names, so a file set opens again after closing. Covered by the extent_files random model with close/reopen and crash ops, SQLite rounds closed after each, and a memory-cell close-and-reopen test; all red before the fix.
<!-- SECTION:FINAL_SUMMARY:END -->
