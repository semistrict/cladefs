---
id: TASK-443.5
title: >-
  Investigate: a restore costs ~13 serial bucket round trips warm (~25 cold) and
  ~10 fsyncs in a row on the shared disk
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 12:24'
updated_date: '2026-11-15 14:48'
labels:
  - perf
  - review-2026-11-15
dependencies: []
documentation:
  - docs/reviews/2026-11-15-sandbox-performance.md
parent_task_id: TASK-443
priority: high
ordinal: 839000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Fork of a point then open of the child: branch::fork_point/finish_point_fork, upload.rs child_free and Shared::start, recovery::recover_layers/state_at, cell::hold, head::take_from, Writer::take, each awaited in turn; the child's record is read three times and its head twice for a branch made milliseconds before. Locally store_dir::write/inherit, Store::make, made_host and Segments::open each sync a file and its directory. Estimated 0.4-0.75 s of bucket time at 30 ms a request and seconds on a busy disk (TASK-333 measured 0.1-1.4 s a sync there). Each fork step also builds a throwaway tokio runtime (upload.rs block_on), so bucket connections it opens may die with it. Found by reading the code, not measured (docs/reviews/2026-11-15-sandbox-performance.md).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The ignored remote::tests::bucket_round_trips_by_phase gains a point-fork and a point-fork-open phase and its counts are recorded; fsync-class calls and their total time around one restore are counted beside 8 VMs writing to the same disk
- [ ] #2 The notes give the measured cost and the workload size at which a customer would feel it; a fix, if needed, is filed as its own task
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Read the fork, open, close, reopen, point and delete paths (host_store, upload.rs, branch.rs, recovery.rs, writer.rs, moves.rs, cell.rs).
2. Add an ignored measurement beside remote::tests::bucket_round_trips_by_phase (cladefs remote/lifecycle.rs, bucket_lifecycle_by_phase): a bucket in memory behind an object store that records every request (kind, key, bytes, start, end) and waits a fixed latency; phases on a base of N extents (CLADEFS_LIFECYCLE_EXTENTS): open of a point and fork of it and the fork's open on a cold host (a bucket object of its own, nothing decoded, new directories), the same warm, a small write and commit, clean close, reopen, snapshot (point) of the written fork, delete; per phase requests by kind, bytes up and down, most in flight, time with a request in flight over the latency (round trips in series), keys read more than once; and the decoded cache after 16 resumes.
3. Run on macOS at ~1,000 and 32,768 extents (larger if under 5 minutes), at 30 ms; record under docs/measurements/.
4. Notes on TASK-443.5 and TASK-443.14: table, verdict per claim, cost per lifecycle; a fix task per real claim. Linux parts (fsyncs beside 8 VMs, killed VM drain, snapshot upload wait) stay open.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Measured on macOS (bucket and request parts only), 2026-11-15: fxdax remote::lifecycle::bucket_lifecycle_by_phase (crates/fxdax/src/remote/lifecycle.rs, ignored; code 08b61a163), results and every request of every phase in docs/measurements/macos-bucket-lifecycle-2026-11-15/ (note.md has the commands). A bucket in memory behind a fixed latency, every request recorded with key, bytes, start, end; bases of 1,024, 32,768 and 262,144 extents; debug build (fxdax tests refuse a release build, TASK-320), so requests, bytes and round trips are reliable, local CPU times only indicative.

| phase | requests | round trips in series | bucket ms at 30 ms / 100 ms a request | bytes up (1,024 / 32,768 / 262,144 extents) | bytes down |
|---|---|---|---|---|---|
| fork of a point (warm or cold) | 6 | 6 | 192 / 611 | 170 | 158 |
| the fork open (warm or cold) | 8 | 7 | 224 / 714 | 112 | 223 |
| point taken and opened on a cold host | 11 | 8 | 256 / 814 | 0 | 4.2 K / 107.9 K / 856.1 K |
| one 4 KiB write + commit | 2 | 2 | 64 / 203 | 641 | 0 |
| clean close after it | 4 | 4 | 128 / 407 | 4.0 K / 107.6 K / 856.7 K | 0 |
| reopen (resume) | 7 | 6 | 192 / 614 | 73 | 4.3 K / 107.9 K / 857.0 K |
| snapshot (point) after a write | 20 | 18 | 574 / 1,835 | 8.6 K / 215.8 K / 1,714 K | 5.1 K / 108.7 K / 857.9 K |
| delete (written, closed) | 9 | 9 | 286 / 917 | 186 | 4.3 K / 107.9 K / 857.0 K |
| restore warm, in all | 14 | 13 | 414 / 1,325 | 282 | 381 |
| restore cold, in all | 25 | 21 | 672 / 2,138 | 282 | 4.6 K / 108.3 K / 856.5 K |
| short life (warm restore, write, close, delete) | 29 | 28 | 890 / 2,850 | 5.1 K / 108.7 K / 857.9 K | 4.7 K / 108.3 K / 857.4 K |
| life with a resume and a snapshot | 67 | 63 | 2,010 / 6,420 | 14.5 K / 325 K / 2,573 K | same |

Never more than 2 requests in flight at once; requests and round trips are the same at every base size.

Verdict, claim 1 (a restore is ~13 serial round trips warm, ~25 cold; record read 3 times, head twice): REAL. Warm: 14 requests in exactly 13 round trips one after another. Cold: 25 requests, 21 round trips (the review's 25 is the request count). The child's record is read 4 times (not 3) and written twice, its head read twice then created. The review's 0.4-0.75 s at 30 ms is 0.41 s warm, 0.67 s cold; at 100 ms 1.3 s and 2.1 s. Nothing grows with the base but the cold host's one download of the base's checkpoint (~3.3 B an extent). Not in the review: taking a point on a cold host opens its store twice (ExtentStore::take_with: create_placed, then host_store::take): 11 requests where 6 would do. Felt at any base size, by every restore, in proportion to the bucket's distance: a customer 100 ms from the bucket waits 1.3 s before the VM's boot begins. The throwaway runtimes (upload.rs block_on) showed no cost against a bucket in memory; what they cost in connections on a real bucket is not measured here. Fix filed: TASK-449.

NOT DONE, not checked: AC #1's fsync half (fsync-class calls and their time around one restore beside 8 VMs writing to the same disk) needs Linux (ubu1), and AC #2 waits for it. This run had no access to ubu1.
<!-- SECTION:NOTES:END -->
