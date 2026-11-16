TASK-443.5, TASK-443.14: what a sandbox's life asks of its bucket, by phase: restore, write, close, reopen, snapshot, delete.

Host: macOS, Apple M5 Pro (host.txt). A bucket in memory behind a fixed latency a request
(`extent_remote::faults::Faulty`), every request to the object store recorded with its key, bytes,
start and end. Debug build: fxdax's tests don't build in release (TASK-320), so only requests,
bytes and round trips are to be relied on; times beside the bucket's are indicative, on a host
other agents were building on.

Run (source.txt):

    export CARGO_TARGET_DIR=/Users/ramon/.t3/worktrees/fxdax/t3-ab73d19b-9b18641383f21458/target
    FXDAX_LIFECYCLE_EXTENTS=1024,32768 cargo nextest run -p fxdax --run-ignored only --no-capture bucket_lifecycle_by_phase
    FXDAX_LIFECYCLE_EXTENTS=262144 cargo nextest run -p fxdax --run-ignored only --no-capture bucket_lifecycle_by_phase
    FXDAX_LIFECYCLE_EXTENTS=32768 FXDAX_LIFECYCLE_LATENCY_MS=100 cargo nextest run -p fxdax --run-ignored only --no-capture bucket_lifecycle_by_phase

`remote::lifecycle::bucket_lifecycle_by_phase` (crates/fxdax/src/remote/lifecycle.rs). A base of N
extents of 1 to 4 blocks, committed 1,024 at a time, closed, and a point made of it, on one "host"
(a `Store` of its own over the objects). Then, on another, which decoded nothing and has no
directory of the point: the point taken from the bucket and opened (`ExtentStore::take_with`, as
the daemon's `take_store`), forked, the fork opened (cold); forked and opened again (warm, the
point open, as the daemon keeps it); one 4 KiB write and its commit; close; reopen; a write; a
point of it (snapshot); deletions. What a host does once, not a restore, is done before: the probe
of conditional writes, a block of object IDs. 262,144 took 351 s, past the 5 minutes a run should:
its phases are done in the first minute, the rest is the 34 resumes below.

## By phase (30 ms a request; bytes at 1,024 / 32,768 / 262,144 extents)

Requests, round trips and the bucket's time are the same at every size. "Round trips" is how many
requests' worth of latency the phase waited, one after another (the time a request was in flight
over the latency); never more than 2 were in flight at once.

| phase | requests | round trips in series | bucket ms at 30 | at 100 | bytes up | bytes down |
|---|---|---|---|---|---|---|
| point taken and opened, cold | 11 (10 GET, 1 HEAD) | 8 | 256 | 814 | 0 | 4.2 K / 107.9 K / 856.1 K |
| fork of the point (cold or warm) | 6 (3 GET, 2 create, 1 update) | 6 | 192 | 611 | 170 | 158 |
| the fork's first open (cold or warm) | 8 (6 GET, 2 create) | 7 | 224 | 714 | 112 | 223 |
| one 4 KiB write and commit | 2 (create, update) | 2 | 64 | 203 | 641 | 0 |
| close after that write | 4 (2 LIST, create, update) | 4 | 128 | 407 | 4.0 K / 107.6 K / 856.7 K | 0 |
| close of a fork that wrote nothing, or after a snapshot | 0 | 0 | 0 | 0 | 0 | 0 |
| reopen | 7 (6 GET, update) | 6 | 192 | 614 | 73 | 4.3 K / 107.9 K / 857.0 K |
| snapshot (point) after a write | 20 (7 GET, 2 ranged, 4 LIST, 5 create, 2 update) | 18 | 574 | 1,835 | 8.6 K / 215.8 K / 1,714 K | 5.1 K / 108.7 K / 857.9 K |
| delete, written and closed | 9 (6 GET, 3 update) | 9 | 286 | 917 | 186 | 4.3 K / 107.9 K / 857.0 K |
| delete, with a snapshot | 10 | 10 | 320 | 1,020 | 202 | 4.4 K / 108.1 K / 857.2 K |
| delete of the snapshot | 8 | 8 | 255 | 816 | 108 | 223 |
| delete, wrote nothing | 9 | 9 | 286 | 917 | 154 | 406 |

| in all | requests | round trips | bucket s at 30 | at 100 | bytes up | bytes down |
|---|---|---|---|---|---|---|
| restore, warm (fork + open) | 14 | 13 | 0.41 | 1.33 | 282 | 381 |
| restore, cold (point + fork + open) | 25 | 21 | 0.67 | 2.14 | 282 | 4.6 K / 108.3 K / 856.5 K |
| a short life: warm restore, a write, close, delete | 29 | 28 | 0.89 | 2.85 | 5.1 K / 108.7 K / 857.9 K | 4.7 K / 108.3 K / 857.4 K |
| with a resume and a snapshot too | 67 | 63 | 2.0 | 6.4 | 14.5 K / 325 K / 2,573 K | 14.5 K / 325 K / 2,573 K |

A checkpoint here is ~3.3 bytes an extent (the collector's fill measured ~4.6,
macos-gc-fork-points-2026-11-14).

## What each phase asks, in order (the REQUEST lines)

- **Fork of a point**, each after the last: the child's record GET (`child_free`); its create; the
  reference's create under the base; the point's record GET; the child's record GET; its update.
- **The fork's first open**: the child's record and head at once; its head again; the base's head;
  its cell GET; the cell's create; the head's create; the record again. The child's record is read
  4 times and written twice in a restore, its head read twice then created.
- **Point taken, cold**: the point's record 3 times, its head (it has none) 4 times, the base's
  head twice, the base's checkpoint once, a HEAD of `gc/marking`: `take_with` opens the point's
  store, drops it and opens it again (6 requests, then 4 more).
- **Close**: a LIST of moves under each key prefix its extents are in (the base's, its own), one
  after the other; the checkpoint's create, of the whole state; the head's update.
- **Reopen**: record and head at once; head; the checkpoint; the cell; the head's update; record.
- **Snapshot**: the child's record; the segment's create and the head's update (its commit); the
  record; then `checkpoint_at`: head, LIST of checkpoints, head, LIST again, the two segments'
  tails at once, the checkpoint's create; the record; the reference's create; the record's create;
  then `writer.checkpoint()`: a LIST of moves for each prefix, the same checkpoint key created
  again (refused, its body sent), the checkpoint read back whole to compare, the head's update.
- **Delete**: head; cell; the head ended; record; the checkpoint, whole, for the forks it names;
  (a child's record, each fork); record; its update; cell; its update.

## The decoded cache (DECODED lines)

34 times: another fork written, closed and reopened (its checkpoint decoded), then a restore from
the point. The restore after the 13th and the 29th read the base's checkpoint from the bucket again
(15 requests, 107.9 K at 32,768, 856.1 K at 262,144): `recovery::Decoded` keeps 16, oldest decoded
out first, and the base's state went although every restore between used it (3 states were
decoded before the count began).

## Reading

- The review's counts hold: a warm restore is 13 round trips one after another (14 requests), a
  cold one 25 requests in 21 round trips. The child's record is read 4 times, not 3; its head
  twice. Its estimate of 0.4-0.75 s at 30 ms is 0.41 s warm, 0.67 s cold; 1.3 s and 2.1 s at 100 ms.
  None of it grows with the base, but for the cold host's one checkpoint.
- Close, reopen, delete and snapshot each move the whole state's checkpoint: for a base of 262,144
  extents a sandbox that wrote 4 KiB uploads 858 K and downloads 857 K to be closed and deleted,
  2.6 MB each way with a resume and a snapshot. At 10^5 short-lived sandboxes a day that is ~86 GB
  up and as much down a day.
- A snapshot is the costliest call: 18 round trips, the checkpoint up twice and down once.
- Filed: TASK-449 (restore's round trips, the point opened twice), TASK-450 (close and reopen, the
  decoded cache), TASK-451 (delete), TASK-452 (snapshot).
- Not measured here, Linux's (ubu1): fsyncs around a restore beside 8 writing VMs (TASK-443.5); a
  killed VM's upload drain before its delete, and a snapshot's wait for uploads with the VM paused
  (TASK-443.14).
