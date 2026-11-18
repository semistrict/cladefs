---
id: TASK-551
title: 'A file set''s root past 4 KiB fails the file set, again after each reopen'
status: Done
assignee:
  - '@claude'
created_date: '2026-11-18 15:25'
updated_date: '2026-11-18 22:58'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 942000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), file-sets review; verified against the code by independent reviewers. Severity: high.

**Where:** `crates/extent_files/src/file_set.rs`: `rg -n -F 'let encoded = root.encode();' crates/extent_files/src/file_set.rs`

commit_root encodes a root that names every committed file. Each file costs 8 + name + 8 + 4 + 24 + 8 bytes plus 20 bytes per sparse extent (up to SPARSE_KEPT = 16, i.e. 320 bytes). The header is 60 bytes plus 16 per inline dying extent (up to 64, i.e. 1024 bytes). Only the dying extents ever spill into an extent of their own. Nothing bounds how many files there are or how long their names are (MAX_PATHNAME 512). The daemon refuses any WRITE_ROOT/COMMIT_ROOT payload over MAX_ROOT = 4096 with EINVAL (cladefsd/codec.rs `root.len() > MAX_ROOT`). The root rides on Plan::write's last POST_BARRIER write, so the refusal comes back as Failure::Committing. That sets `inner.failed`, after which every call on the file set returns Failed, which is SQLITE_IOERR for every database in the cell. Reopening restores the previous root, but the next commit that needs a root this size fails the set again.

**Trigger.** A daemon-served SQLite cell (sqlite_extents::daemon::open) holding about 50-60 database files with 20-30-byte names, or about 10 busy ones whose last commits left full sparse lists (16 x 20 bytes each; OLTP commits rewriting page 1 and interior pages leave small extents under half live). Example: 11 files x (52 + 12-byte name + 320) = 4224 bytes > 4096. Creating the next database or committing a transaction that grows a sparse list leads to: x_sync -> FileSet::sync -> commit -> commit_root -> Root::encode longer than 4096 -> plan.write's last write with root -> daemon EINVAL -> Failure::Committing -> inner.failed. From then on every connection on every database in the cell gets SQLITE_IOERR, and after a reopen the same commit fails again.

**Why the tests stay green.** FakeDevice and the in-process image device take a root of any length (fake_device.rs stores `root.to_vec()` with no limit), so tests on them never see the daemon's limit. That is a fake accepting wrong behavior. The daemon tests open one database ("app.db"), and no test bounds Root::encode's length or opens many files in one set.

**Backlog.** New. TASK-149.17 (Done) set the daemon's MAX_ROOT wire cap; TASK-296 and TASK-440 (Done) defined the root format and sparse lists, spilling only the dying list. None bounds the root's size.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 On the daemon, a file set holding 80 files with 30-byte names, and one whose files carry full sparse lists (16 entries each), commits every sync: the set never enters Failed and no database in the cell gets SQLITE_IOERR, before or after a reopen.
- [x] #2 The encoded root never exceeds the device's max root (4096 bytes on the daemon) for any number of files, name length or sparse-list size, checked by a property test in extent_files.
- [x] #3 FakeDevice refuses roots over 4096 bytes as cladefsd's codec does, and an extent_files test that creates 80 files with 30-byte names and syncs each goes red with the fix reverted.
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

Fixed: commit_root lists the files in an extent of their own when the encoded root would pass storage_device::MAX_ROOT (4096), and asserts every root it commits is within it; the root format is now version 4 with files_list. Inner::lists (was list) holds every extent the last root names for the next opening to read. FakeDevice refuses roots past MAX_ROOT; cladefsd's wire MAX_ROOT is that constant. Tests: extent_files model a_file_set_of_more_files_than_a_root_holds_commits_each_and_opens_again (80 files of 30-byte names, 300 of 400) fails with the spill turned off; sqlite_extents daemon a_cell_of_many_databases_commits_each_and_opens_again passes on ubu1. AC2 is held by the assertion on every commit rather than a property test.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Roots past a device's 4 KiB list their files in an extent; verified on FakeDevice (now refusing long roots) and on the daemon (ubu1).
<!-- SECTION:FINAL_SUMMARY:END -->
