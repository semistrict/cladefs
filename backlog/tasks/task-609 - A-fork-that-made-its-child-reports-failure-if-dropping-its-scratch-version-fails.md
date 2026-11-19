---
id: TASK-609
title: >-
  A fork that made its child reports failure if dropping its scratch version
  fails
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18 15:32'
updated_date: '2026-11-19 06:43'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 1000000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), cells review; verified against the code by independent reviewers. Severity: low.

**Where:** `crates/cladefs/src/host_cell.rs`: `rg -n -F 'Ok(dropped?)' crates/cladefs/src/host_cell.rs`

`fork_through` makes the child (its head in the bucket and its store at `path`) through the `fork(...)` call, then drops the scratch version and returns `forked?; Ok(dropped?)`. If `uploader.drop_versions` fails after the fork succeeded, the whole fork returns Err, even though the child cell is live in the bucket and its store sits whole at `path`. Nothing unforks or removes it on this path. `Writer::drop_versions` can fail with the writer already stopped, or with a head swap that failed: a bucket error past its retries, or a precondition lost to a later placement (Fenced). In both cases `change_versions` also stops the writer. The doc on `OpenCell::fork` promises the opposite: "A fork that fails is deleted in the bucket, as the fork's alone, and leaves nothing at `path`". The caller is told the fork failed, and the child it never learns about keeps every extent it inherited away from the collector until an operator deletes it.

**Trigger.** 1) The VM manager calls ForkCell on a store (cladefsd `fork_held`, which calls `fork_quiescent` for a live store or `fork` for a kept one), or calls `Storage::fork_to`/`Filesystem::fork_storage_to` on an in-process image. 2) `fork_at_version` runs `commit_version`, then `make_uploaded`, then `host_cell::fork_through`. 3) In `fork_through`, `fork(&at.store, &at.uploader, path, child, prepare)` succeeds: `Uploader::forked` writes the child's head and `make_child` makes and syncs the store at `path`. 4) `uploader.drop_versions({version})` then fails because `head::swap` hits a bucket error or a precondition failure (or the writer was stopped meanwhile). 5) `fork_through` returns Err (EIO/EROFS through `Error::from(Failure)`). `OpenCell::fork` returns Err, and cladefsd answers ForkCell with an error. For a kept store, `fork_from_store` retries `fork_held`; the second `host_cell::fork` gets EEXIST from `create_dir(path)` and the caller sees EEXIST. 6) A complete child exists at `to` and is live in the bucket, but the caller has no cell ID for it and believes the fork failed. The collector keeps everything the version held.

**Why the tests stay green.** `fork_faults::each_write_of_a_fork_going_wrong` does reach the drop's head-swap write with `Mishap::Refused`, but `World::settle` treats any fork that returned Err like a dead host's (`Leftover::Deleted`): it deletes whatever is at the path (`self.delete(&path, case)`) and lets `delete_orphans(.., failed=true, ..)` delete the live child. It never asserts that a fork returning Err left nothing at the path and no live child. `FORK_STEPS` has no failpoint after `fork::made`, so no step-failure case reaches the drop either.

**Backlog.** Related to TASK-514 (To Do), which covers a fork's version outliving a host that dies mid-fork, not a made child reported as failed.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A fork whose child was made (its head is written and its store sits at the path) returns Ok with the child's cell ID even when dropping the scratch version fails afterwards.
- [ ] #2 A fork that returns Err while its host is alive leaves nothing at the child's path and no live child in the bucket.
- [ ] #3 fork_faults (crates/cladefs/src/bucket/fork_faults.rs) gets a failpoint, or a Write mishap, that fails the scratch version's drop in fork_through. World::settle asserts the second criterion for every failed fork on a live host. Both go red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
