---
id: TASK-553
title: >-
  A failed remap recycles a file's region while SQLite holds pages fetched from
  it
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18 15:25'
updated_date: '2026-11-18 22:36'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 944000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), file-sets, unsafe-atomics review; verified against the code by independent reviewers. Severity: high.

**Where:** `crates/extent_files/src/file_set.rs`: `rg -n -F 'Ok(()) => regions.push(region),' crates/extent_files/src/file_set.rs`
**Where:** `crates/extent_files/src/file_set.rs`: `rg -n -F 'Ok(()) => regions.push(region),' crates/extent_files/src/file_set.rs`

FileSet::remap's error branch (crates/extent_files/src/file_set.rs) takes the region away from the file (set_region(None), file.region = None), unmaps the whole region through the window (DaemonWindow::unmap, which sends the daemon an UNMAP), and pushes it back onto inner.regions for the next file opened. It never checks file.fetches. The file set promises, in its module docs and in refresh's `file.fetches == 0` guard, that nothing in a file's region is mapped again while fetched pages are held. refresh keeps that promise. map_reached does not: fetch and read both call it whenever a request reaches a chunk that is not mapped yet, which has been the normal path since TASK-440 made mapping lazy, and it calls remap whatever fetches is. The error can come from file.pages.runs reading a cold tree node from the device, or from window.map. DaemonWindow::map calls CellClient::map_many, which fails on a socket error. In the daemon, OpenCell::map_many returns mapped_of's errno: ENOENT, EIO from a base's cached(), or EINVAL. After the failure, remap only logs 'read from the device from now on'. fetch returns None and SQLite falls back to xRead, but the pointers xFetch already gave it stay held until xUnfetch. Trigger: a daemon-served file set with PRAGMA mmap_size. A statement or b-tree cursor on a.db holds a fetched page (fetches > 0), possibly across sqlite3_step calls. One connection is enough. A further xFetch or xRead of a.db reaches an unmapped chunk while the bucket is failing (for example the bucket soak's failing bucket), and the remap fails. On A's next touch through its held pointer, the second stage in the process finds the page maps nothing (resolve_through gets ENOENT and calls restrict(PROT_NONE)), and the process dies of SIGSEGV. If another file opens first (b.db, or an ATTACH), open_file pops the recycled region (the free list is LIFO) and b's reads map b's chunks at the same addresses. A's held pointer then silently reads b's committed pages as its own. If that page is an interior b-tree page used to place an insert, or it feeds an INSERT ... SELECT, the corruption is committed. Why tests stay green: the tests' CopyingWindow::map reads from a FakeDevice that never fails there, no failpoint covers FileSetWindow::map or unmap, and CopyingWindow's unmap fills 0xab, so a dangling fetch shows only as garbage nothing asserts. a_commit_waits_for_fetched_pages_to_remap covers only the commit path (refresh, which checks fetches). No test holds a fetched page across a failed window map or tree-node read, or opens more files after one.

**Backlog.** Related to TASK-440 (Done), whose lazy mapping lets remap run while pages are held, and to TASK-300 (Done), whose AC covers only a commit waiting for fetched pages. TASK-530 (To Do) is a different region leak, on an uncommitted close.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 While a file has fetched pages held, a failed remap (a window map or a tree-node read failing) leaves the file's region mapped and owned by that file. The held pointer still reads the file's committed bytes, and the process does not fault.
- [ ] #2 A file opened after such a failure never gets a region whose fetched pages are still held.
- [ ] #3 A test in crates/sqlite_extents/tests/sqlite.rs (Mode::Window) makes the window map or a device read fail on demand. It holds a fetched page of file A across the failure, then opens and reads file B, and asserts that A's pointer still reads A's bytes. It goes red (0xab) with the fix reverted.
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
<!-- SECTION:NOTES:END -->
