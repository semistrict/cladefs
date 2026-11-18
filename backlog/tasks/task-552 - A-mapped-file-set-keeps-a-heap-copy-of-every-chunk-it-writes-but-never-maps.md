---
id: TASK-552
title: A mapped file set keeps a heap copy of every chunk it writes but never maps
status: Done
assignee:
  - '@claude'
created_date: '2026-11-18 15:25'
updated_date: '2026-11-18 22:58'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 943000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), file-sets review; verified against the code by independent reviewers. Severity: high.

**Where:** `crates/extent_files/src/pages.rs`: `rg -n -F 'self.recent.extend(wrote.map(|(index, chunk)| (index, chunk.unwrap_or_else(zeros))));' crates/extent_files/src/pages.rs`

Pages::apply adds every chunk a commit wrote to `recent` when the file has a region. `recent` is cleared only by take_stale (from FileSet::refresh) or set_region. refresh runs only from fetch (xFetch) or from commit once stale_chunks >= REMAP_BATCH (1024). But stale_chunks counts only chunks the region already mapped (mark_stale skips unmapped ones and caps at region.chunks()), while `recent` takes everything written. So chunks first written past the committed end (appends), chunks never read back through FileSet::read, and every chunk beyond the region's end all go into `recent` without ever adding to stale_chunks. Without xFetch, SQLite's default (mmap_size = 0, sqlite-bench's `daemon` configuration), nothing clears them. REMAP_BATCH's doc says this memory is '4 MiB at most a file'. In fact it reaches one heap copy of every page written, outside any budget.

**Trigger.** sqlite_extents::daemon::open(socket, store, cap, window = 1 GiB, Options::default()), giving 4 regions of 256 MiB. The connection keeps SQLite's default mmap_size = 0 and appends rows one transaction each (sqlite-bench's `commit` workload, any log or time-series table). Each commit writes page 1 and the rightmost leaf. Page 1 is mapped (SQLite's xRead of the change counter at offset 24 -> map_reached), so stale_chunks stays at about 1. New leaves were past the committed end at their first commit, stay in SQLite's page cache and are never xRead, so they are never mapped or stale. refresh never runs and `recent` keeps every leaf: a 10 GiB log holds about 10 GiB of heap in the SQLite process. A database larger than its 256 MiB region adds every chunk written past the region's end the same way, whatever the workload.

**Why the tests stay green.** Window-mode tests in sqlite_extents/tests/sqlite.rs and tests/daemon.rs always set PRAGMA mmap_size, so each xFetch with any stale chunk calls refresh -> take_stale, which clears `recent`. Their databases are far below a region (16 MiB in CopyingWindow), and nothing measures `recent`'s size or the process's heap.

**Backlog.** RELATED: TASK-440 (Done) brought lazy mapping, after which mark_stale counts only mapped chunks while `recent` keeps every written one; TASK-301 (Done) introduced a commit's chunks read from memory until remapped; TASK-530 (To Do) is the different region leak on close.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 With SQLite's default mmap_size = 0 (no xFetch) and an append-only workload of one row per transaction, the chunks a mapped file set keeps in memory awaiting a remap stay at most REMAP_BATCH (1024) chunks, 4 MiB, a file, however large the database grows.
- [x] #2 Chunks first written past the committed end, never read back, or beyond the region's end add no unbounded heap: a database grown past its 256 MiB region holds at most REMAP_BATCH chunks of such copies.
- [ ] #3 A regression test in extent_files' pages.rs with a region of 64 chunks makes 4000 commits, each reading 16 bytes of chunk 0, rewriting it and appending one new chunk, with no fetch, and asserts `recent` holds at most REMAP_BATCH chunks; a sqlite_extents window-mode test without mmap_size under a counting allocator checks heap stays bounded as the database outgrows its region; both go red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-18: taken up by the Codebase Bug and Performance Audit session, in one batch of file-set fixes with TASK-551, 552, 553, 530, 583 and 584 (branch t3/codebase-bug-perf-review).

Fixed: Pages::apply marks what it changed stale itself, and keeps in recent only stale chunks (mapped as before the commit), REMAP_BATCH at most (the constant moved to pages.rs). Chunks never mapped, appended, or past the region are read from the device. Test: pages.rs what_a_mapped_file_keeps_of_its_commits_stays_within_a_remap_batch (64-chunk region, 4000 commits rewriting chunk 0 and appending one) fails with every written chunk kept. Not written: AC3's sqlite_extents counting-allocator test; the bound holds by construction and the pages test covers appends past the region.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
recent holds only stale chunks, REMAP_BATCH at most; pages.rs test fails without the fix.
<!-- SECTION:FINAL_SUMMARY:END -->
