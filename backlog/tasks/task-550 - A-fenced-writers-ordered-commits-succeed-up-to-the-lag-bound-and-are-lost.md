---
id: TASK-550
title: 'A fenced writer''s ordered commits succeed up to the lag bound, and are lost'
status: To Do
assignee: []
created_date: '2026-11-18 15:24'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 941000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), durability-end-to-end, cross-host-take-restore review; verified against the code by independent reviewers. Severity: high.

**Where:** `crates/cladefs/src/open_cell.rs`: `rg -n -F 'Commit::Ordered => match self.uploader.as_ref() {' crates/cladefs/src/open_cell.rs`
**Where:** `crates/cladefs/src/open_cell.rs`: `rg -n -F 'upload_wait(epoch).run(|| uploader.wait_lag(epoch).map_err(upload_errno))' crates/cladefs/src/open_cell.rs`

Once another placement takes a cell, the old store's uploader stops for good with Failure::Bucket(Fenced, Stale or Deleted), yet ordered commits on the old store keep succeeding. OpenCell::commit_and_wait (crates/cladefs/src/open_cell.rs) answers Commit::Ordered with uploader.wait_lag alone. Shared::ask (crates/local_cell/src/upload.rs) returns Ok whenever the last uploaded commit is within max_lag (4096 commits) and the write-ahead log within max_lag_bytes (1 GiB). It never reads progress.failed; only Shared::reached looks at the failure, and only past the bound. Ordered commits are every guest fsync, fdatasync, msync and O_SYNC write, plus the journal's own POST_BARRIER writes. may_write() checks only the role and LocalCell::writable() only closed or failed, so the old store keeps taking WRITEs too. The code disagrees with itself: Commit::Local checks self.uploading() after its write to disk and returns EROFS, and KeptImage::committed checks uploader.failed() before wait_lag, while the weaker Ordered level checks nothing. These commits can never reach the cell. The new writer's head has moved on, and the old store's next opening refuses its log as Diverged or Stale (ESTALE) until `cladefsctl take` drops it. Nothing else watches uploader.failed(), so the VM is not stopped, and with no time bound a guest that commits rarely goes on for a long time. This goes against doc-4 (EROFS on WRITE, FREE and COMMIT once another host took the head), doc-7 (a fenced writer answers EROFS from then on, its VM process stopping the guest), TASK-39.7 AC #1 (the fenced writer's commits then fail) and TASK-28.5's plan (EROFS once another writer takes the cell). Trigger: VM A on host H1 writes cell C, and H2 takes C with a higher fencing token while H1 is cut off from the control plane but still reaches the bucket. The take can come from OpenCell::place then a reopen, from CreateCell(cell=C, token) restarting the VM on H2, from `cladefsctl take`, or from a take-based move whose source was not stopped. Within about one upload interval (1 s), H1's Writer::acknowledge calls head::swap, which fails with Error::Fenced, and Shared::fail sets progress.failed. A guest app such as Postgres then calls fsync: cladefs_vfs_fsync writes back, Fxfs::fsync syncs the journal, and the device sends WRITE|WRITE_COMMIT without UPLOADED. The daemon's write_with_barriers runs commit_with(Ordered): commit_ordered succeeds and wait_lag returns Ok because the lag is small, so fsync returns 0. This repeats for up to 4096 commits or 1 GiB of write-ahead log, seconds of pgbench transactions, all lost. Only COMMIT_UPLOADED (syncfs, sync, unmount, fsfreeze) and COMMIT_LOCAL get EROFS. Commits ordered before the uploader notices the fence (about one upload interval) are an accepted loss; the defect is everything answered after it. Tests stay green because every fencing test commits at the Uploaded level or calls wait_uploaded: bucket.rs durable_commits_fail_once_another_takes_the_cell and later_placements_fence_earlier_ones, and upload.rs stores_take_their_cells_and_are_fenced_with_them. Both bucket.rs tests even write after the fence without checking the result and expect it to succeed. waits_at_the_lag_bound makes ordered commits only against a failing bucket, not a fenced writer, and asserts uploader.failed() == None. specs/bucket's NoAckedLost counts only commits a head swap names, so ordered and local answers to the guest are outside the model. Fix: check self.uploading() in the Ordered arm, or have ask and wait_lag check the failure first.

**Backlog.** Regresses TASK-39.7 (Done; AC #1: a fenced writer's commits then fail) and TASK-28.5 (Done; EROFS once another writer takes the cell). Both were tested only at the Uploaded level. TASK-364 added the uploader-failure check to Local commits only, and TASK-314's stall at the lag bound covers a failing bucket, not a fenced writer.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 After another placement takes the cell and the old store's uploader has failed with Fenced, Stale or Deleted, an ordered commit on the old store (a guest fsync: COMMIT without COMMIT_UPLOADED) returns EROFS. WRITE and FREE to it return EROFS too, as doc-4 says.
- [ ] #2 Regression tests in crates/cladefs/src/bucket.rs, next to later_placements_fence_earlier_ones and durable_commits_fail_once_another_takes_the_cell, wait until failed() is Some(Fenced). They then assert commit_ordered(None) == Err(EROFS) and that a write to a new extent fails with EROFS, with a variant for Stale. A cladefsd test asserts that a COMMIT without COMMIT_UPLOADED after a takeover is answered EROFS. Each goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
