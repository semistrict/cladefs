# Log appends since TASK-433.2: store-bench A/B on ubu1 (TASK-433.7), 2026-11-15

Three sides, each built in its own worktree and target (`--release`, store-bench), each binary
checked for a side-only string; run in one `fxjob --bench` job, interleaved A B C, C B A, A B C,
C B A, `TMPDIR` on `/mnt/fxdax-scratch` (ext4 on a ZFS zvol, `/dev/zd0`). Raw output in
[ubu1-log-appends-ab-2026-11-15/](ubu1-log-appends-ab-2026-11-15/).

- A `39f34921c`: TASK-433.2's parent, logs written through the host's page cache.
- B `0ef6fa1b4`: TASK-433.2, logs `O_DIRECT`, one write and one `set_len` per record.
- C `c78d0dbf2`: main, the stores' own page cache in every directory (TASK-433.1, 433.3, 433.5).

Medians of rounds 2-4 (round 1 of every side is cold, 3-8x slower, and left out):

| | A | B | C |
|---|---|---|---|
| `durable`: flush of a commit of 2 extents (100 each round) | 5.6 ms | 6.3 ms | 5.4 ms |
| `burst`: the commit's flush, ~1024 records | 42.8 ms | 46.6 ms | 235.2 ms |
| `burst`: a write of 256 KiB | 295 µs | 297 µs | 690 µs |

Ranges (rounds 2-4): durable A 5.4-5.8, B 5.1-6.5, C 5.2-5.7 ms; burst commit A 39.7-43.6,
B 44.9-82.7, C 117.9-242.7 ms; burst write A 288-307, B 295-369, C 641-720 µs.

B against A, TASK-433.7's question: the per-record direct write and truncate add no flush
latency that grows with records per flush beyond the noise (+3.8 ms over ~1024 records, inside
B's spread): no change made. C against B: `burst`'s writes 2.3x and its commit 5x slower, a
regression since TASK-433.2, filed as its own task.
