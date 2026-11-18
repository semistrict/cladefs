---
id: TASK-582
title: 'Local commits ignore the lag bound, so the write-ahead log grows without limit'
status: To Do
assignee: []
created_date: '2026-11-18 15:29'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 973000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), durability-end-to-end review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/cladefs/src/open_cell.rs`: `rg -n -F 'Commit::Local => self.flush().and_then(|()| self.uploading()),' crates/cladefs/src/open_cell.rs`

`commit_and_wait` applies the uploader's lag bound (`wait_lag`: 4096 commits or 1 GiB of write-ahead log) only to `Commit::Ordered`. `Commit::Local` flushes to disk and checks `uploading()` (whether the uploader failed for good). Bucket outages are retried for ever and never set `failed` (TASK-314), so a Local commit always returns as soon as it is on disk, however far uploads have fallen behind. Every commit `DaemonDevice` makes is Local: `commit_root`, `flush` and POST_BARRIER writes (crates/cladefs/src/cladefsd/device.rs:89, 128, 138), and so is every commit of `sqlite_extents` and `memory_cell`. This breaks two documented bounds. doc-12 says "Past the bound, commits wait: the lag is what a lost host can cost, and it stays within the bound". doc-14 says "The host's disk is bounded by the lag bound (Options::max_lag_bytes...)". It also inverts the level order ("each including the one before"): the stronger Local level waits less than Ordered at the bound. doc-21 notes the fact in passing ("Today it applies only to Ordered commits (wait_lag)") and fixes it only for replicated cells (TASK-537.3). Unreplicated Local commits stay unbounded.

**Trigger.** 1. The daemon serves a SQLite cell (`sqlite_extents::daemon`, DaemonDevice), or a VM snapshot writes a memory cell (`memory_cell`, DaemonDevice). 2. The bucket fails for a long time: S3 is out of service, full, or refusing credentials. Uploads retry with backoff, and `Progress::failed` stays None. 3. Each SQLite transaction's commit_root sends COMMIT_LOCAL. The daemon runs `store.commit_ordered`, `flush()` and `uploading()`, all Ok. No `wait_lag`. 4. Commits go on past 4096 and past 1 GiB of WAL. Closed segments pile up in the store's write-ahead log on the host's disk, bounded only by the tenant's disk limit if any; the default tenant has none. A VM snapshot's multi-GiB memory image goes the same way. 5. If the host is lost now, every commit since the outage began is lost, far beyond the configured lag that doc-12 promises a lost host costs at most. Meanwhile the host disk can fill, failing every other store on it.

**Why the tests stay green.** The lag-bound tests in crates/cladefs/src/bucket.rs (`commits_wait_at_the_lag_bound`, `..._in_bytes`, `..._while_the_bucket_is_full`, through `waits_at_the_lag_bound`) commit only with `commit_ordered`. The only `commit_local` tests are in bucket/write_budgets.rs and measure disk writes with the bucket healthy. Nothing commits Local with the bucket shut past `max_lag`.

**Backlog.** Related: TASK-301 (Done) added COMMIT_LOCAL with no lag bound. TASK-314 and TASK-28.5 (Done) bound only Ordered commits. TASK-537.3 (To Do) closes the gap for replicated cells only.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 While the bucket is shut, Local commits stall at the lag bound like Ordered ones. This covers COMMIT_LOCAL and every commit the daemon device makes for SQLite and memory cells. Past max_lag commits, or past max_lag_bytes of write-ahead log plus the last commit, the next commit waits until uploads catch up.
- [ ] #2 Tests in crates/cladefs/src/bucket.rs add a waits_at_the_lag_bound variant that uses commit_local(None) with max_lag 2 and the gated bucket shut. It asserts the third commit blocks (not finished, uploader.stalls() > 0) and completes once the bucket opens. A max_lag_bytes variant and a sqlite_extents daemon test with a transaction loop against a shut bucket are added too. All go red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
