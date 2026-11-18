TASK-449: a restore (a fork of a point and the fork's first open) asks the bucket what the protocol
needs. Same benchmark as TASK-443.5's (`remote::lifecycle::bucket_lifecycle_by_phase`,
docs/measurements/macos-bucket-lifecycle-2026-11-15), with one column more: `chain`, the longest
chain of requests each begun once the one before it was answered, which is the round trips one
after another counted, where `in-series` estimates them by the time a request was in flight over the
latency (debug build; host in host.txt).

    export CARGO_TARGET_DIR=/Users/ramon/.t3/worktrees/fxdax/t3-ab73d19b-9b18641383f21458/target
    CLADEFS_LIFECYCLE_EXTENTS=1024,32768 cargo nextest run -p cladefs --run-ignored only --no-capture bucket_lifecycle_by_phase
    CLADEFS_LIFECYCLE_EXTENTS=1024,32768 CLADEFS_LIFECYCLE_LATENCY_MS=100 cargo nextest run -p cladefs --run-ignored only --no-capture bucket_lifecycle_by_phase

Files: before-30ms-1024-32768.txt, before-100ms-1024-32768.txt (main at 80e84f8b9, no `chain`
column: it came with this change); after-30ms-1024-32768.txt, after-100ms-1024-32768.txt.

## The restore (the same at 1,024 and 32,768 extents but for the cold host's one checkpoint)

| | requests | round trips in series | bucket ms at 30 ms | at 100 ms | bytes up | bytes down (1,024 / 32,768) |
|---|---|---|---|---|---|---|
| warm, before | 14 | 13 | 418 | 1,327 | 286 | 389 |
| warm, after | **7** | **6** | 192 | 614 | 286 | 138 |
| cold, before | 25 | 21 | 673 | 2,145 | 286 | 4,636 / 108,279 |
| cold, after | **11** | **9** | 289 | 921 | 286 | 4,158 / 107,802 |

By phase, after (each the same at both sizes and latencies):

| phase | requests | in series | what, in order |
|---|---|---|---|
| point taken and opened, cold | 4 (was 11) | 3 (8) | the point's record and the collector's marking, at once; the base's head; the checkpoint at the fork point |
| fork of the point | 5 (6) | 4 (6) | the clone's record created; its reference and its cell held, at once; the point's record; the clone's record updated |
| the fork's first open | 2 (8) | 2 (7) | its head created; its record |

Other phases changed by the same code: a reopen, 6 requests in 4 round trips (7 in 6 before: the
record, head and cell read at once, the head read once); a deletion of a store that wrote nothing,
8 in 8 (9 in 9); a snapshot's deletion, 7 in 7 (8 in 8). A whole short life (warm restore, a
write, close, delete) 22 requests in 21 (29 in 28); with a resume and a snapshot 58 in 53 (67 in 63).

## Connections

`cladefs remote::lifecycle::restores_connect_to_the_bucket_no_more` (S3 server in memory, counting
the connections it accepts; the host a client of its own): with `upload::block_on` on a runtime of
each call's own, as before, each restore's fork opened a connection, `[(1, 0), (1, 0), (1, 0)]` (fork,
open) over three restores; on one runtime for the process, none. `extent_remote
store::tests::requests_on_runtimes_dropped_after_connect_anew`: a GET on a runtime of its own each
time connects anew every time (`[1, 1, 1]`), on one runtime once (`[1, 0, 0]`). On a real bucket a
connection is a TCP and a TLS handshake, one or two round trips, before its first request.
