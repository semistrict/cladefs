---
id: TASK-443.14
title: >-
  Investigate: a short-lived sandbox uploads all it wrote and a whole-state
  checkpoint before it can be deleted; snapshot and delete pay for the
  checkpoint again
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 12:26'
updated_date: '2026-11-15 14:51'
labels:
  - perf
  - review-2026-11-15
dependencies: []
documentation:
  - docs/reviews/2026-11-15-sandbox-performance.md
parent_task_id: TASK-443
priority: medium
ordinal: 848000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/cladefs/src/store_table.rs cuts a closing store's upload drain short for a Claim::Writer alone, so a delete waits CLOSING_WAIT and returns EBUSY while the dead VM's store uploads up to its lag bound; crates/extent_store/src/upload.rs writes a checkpoint of the whole state, inherited extents included, on every clean stop, after one serial LIST per key prefix (MovesMaps::read). A resume then downloads and decodes that checkpoint (recovery.rs keeps 16 decoded, first in first out); branch::delete_under recovers the whole state to list forks; a snapshot waits for the backlog with the VM paused (take_snapshot -> wait_uploaded), then re-reads its own state and writes its checkpoint twice (recovery::checkpoint_at, writer.checkpoint). Found by reading the code, not measured (docs/reviews/2026-11-15-sandbox-performance.md).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 For a fork of a realistic base: write 500 MiB, kill the VM, delete (time to success, bytes uploaded after the kill); open, one write, close (bucket bytes and requests); snapshot after N GiB written (upload_wait); delete (downloaded bytes)
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

Verdicts:
- Claim 2 (every clean close of a store that uploaded writes a whole-state checkpoint after a LIST per key prefix; the reopen downloads and decodes it; the 16-entry decoded cache is first in, first out): REAL. Close after one 4 KiB write (a 584-byte segment): 2 LISTs (the base's prefix, its own), one after the other, then the checkpoint of every extent, inherited included: 107.6 K at 32,768 extents, 856.7 K at 262,144. Reopen reads it back. A fork that wrote nothing closes for 0 requests. The decoded cache: with other stores resumed between restores of one point, the restore after the 13th and the 29th resume downloaded the base's checkpoint again (16 apart; 3 states were decoded before the count), though every restore in between used it: one request and the checkpoint's bytes more, and its decode. Fix filed: TASK-450 (TASK-242, delta checkpoints, is the related format change).
- Claim 3 (a delete recovers the whole state only to list forks, ~9 serial round trips): REAL, exactly 9 requests one after another (10 with a snapshot of it), and the whole checkpoint down (857 K at 262,144). Fix filed: TASK-451.
- Claim 4 (a snapshot re-reads the store's own state and writes its checkpoint twice): REAL, with a correction: the state it re-reads is its head twice, two LISTs of its checkpoints and its segments' tails since its last checkpoint (the checkpoint itself came from the decoded cache here; a download more if not there); the checkpoint is then created twice at the same key (checkpoint_at, then writer.checkpoint(): the second refused, its body sent) and read back whole once (bucket::create comparing): 2x its bytes up, 1x down, 20 requests in 18 round trips, 0.57 s at 30 ms, 1.8 s at 100 ms, with the VM paused in production. Fix filed: TASK-452.

Cost per sandbox: a short life (warm restore, one write, close, delete) is 29 requests, 28 round trips, and for a base of 262,144 extents 858 K up and 857 K down for 4 KiB written; with a resume and a snapshot 67 requests and 2.6 MB each way. At 10^5 short-lived sandboxes a day on such a base: ~86 GB up and as much down a day, and 2.9 M requests. Felt as cost from bases of ~30k extents up (108 K each way a sandbox), as latency at any size (close 0.13 s, delete 0.29 s, snapshot 0.57 s at 30 ms).

NOT DONE, not checked: AC #1's Linux parts (write 500 MiB, kill the VM, delete: time and bytes uploaded after the kill; upload_wait of a snapshot after N GiB written, with a VM) need ubu1, which this run had no access to; AC #2 waits for them.
<!-- SECTION:NOTES:END -->
