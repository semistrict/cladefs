---
id: TASK-612
title: Making a version succeeds after the store's uploader has failed for good
status: Done
assignee:
  - '@claude'
created_date: '2026-11-18 15:33'
updated_date: '2026-11-19 10:48'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 1003000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), cells review; verified against the code by independent reviewers. Severity: low.

**Where:** `crates/cladefs/src/open_cell.rs`: `rg -n -F 'let id = self.at_rest(|| self.commit_version(keep_last, true))?;' crates/cladefs/src/open_cell.rs`

Once the uploader stops for good (`Uploader::failed()`: Fenced when a later placement took the cell, Deleted, Corrupt, or a bucket error past its retries), the store's commits "are its cell's no more". For that reason `Commit::Local` checks `self.uploading()` and fails rather than let a local-only commit pass for one the bucket will hold. `make_version` gives the same promise ("durable here when this returns ... uploading it and naming it in the head come later, in the background"), but it never checks `uploading()`. `LocalCell::commit_version` only checks that the store is writable, and an uploader failure doesn't make it unwritable. So MakeVersion returns an ID for a version that this uploader will never upload and the head will never name. A VM snapshot record built on it fails only at restore time.

**Trigger.** 1) A host's placement of a cell is superseded (another host takes the cell with a higher fencing token), or its uploader otherwise fails permanently. Its uploader records `progress.failed`, but the store stays open. 2) The VM manager on the old host snapshots the VM: MakeVersion calls `OpenCell::make_version`, `at_rest` and `commit_version` succeed locally, and the ID is returned. 3) The snapshot record names the version. A restore elsewhere gets ENOENT from `fork_of_version`; a restore on this host fails in `opened_version`'s `sync`. The failure surfaces only then, not at the snapshot.

**Why the tests stay green.** The version tests (open_cell, cladefsd tests around `make_version`) all run with a healthy uploader. `each_uploader_failure_has_its_errno` checks only the errno mapping. No test makes a version after `uploader.failed()` is set.

**Backlog.** No related task covers it: TASK-476 and TASK-508.2 (In Progress) own local-first versions but name no check of a failed uploader.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 MakeVersion on a store whose uploader has failed for good (fenced by a later placement, deleted, corrupt, or out of retries) fails with EROFS, as Commit::Local does, and returns no version ID.
- [x] #2 A regression test in open_cell's tests lets another placement take the cell (as stores_take_their_cells_and_are_fenced_with_them does), waits for uploader.failed(), asserts make_version(None) fails with EROFS, and goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
make_version passes OpenCell::admit through at_rest, which refuses once the uploader failed for good. Its test is bucket.rs's fenced_stores_change_nothing (and stores_of_deleted_cells_change_nothing), not open_cell's: they take the cell from another placement as the AC asks. macOS: clippy, workspace 2712/2712, sim 41/41; Linux (ubu1, 808ed60ca): workspace 3044/3046, the fork-steps test updated since and passing alone (43a0c9ae2), windows_are_where_snapshots_had_them failing once, passing 55 runs after (TASK-655); scripts/upstream-changes.py --check. Hand mutants (target/b2-mutants.py), each caught by its test.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
MakeVersion on a store whose uploader failed for good fails with EROFS, through OpenCell::admit. Tested by bucket.rs fenced_stores_change_nothing; a mutant keeping make_version past the gate fails it.
<!-- SECTION:FINAL_SUMMARY:END -->
