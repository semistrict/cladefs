# store-bench burst through the stores' page cache on ubu1 (TASK-433.8), 2026-11-16

`store-bench burst`: 1024 writes of 256 KiB, then one commit, which frames 3 segments. All on
ubu1 in `fxjob --bench` jobs, sides interleaved (A B C, C B A, ...), six rounds, each side built
in its own worktree and target and checked for a side-only string; medians of rounds 2-6 (round
1 cold). Two disks: ext4 on the scratch zvol (`/dev/zd0`, which drifts up to 4x within a run:
`dd oflag=direct` fell from 939 to 208 MB/s in one), and ext4 over a RAM-backed loop device
(`/dev/shm`), which takes the disk's drift out.

Sides: p `150b82934` (the code of `0ef6fa1b4`, data through the host's page cache); main before
the fixes; f `f6eb96991` (write behind begins write-back); g `2e0952e1e` (reused aligned
buffers); h `3e0256500` (the cache sized to what the governor leaves it, store-bench governing
once as the daemon's first look does).

## Cause

- Bisect: the regression came with `3139c4bdd` (TASK-433.1/433.3), the stores' own page cache.
- Not write amplification: p and main both write 260 MiB to the device; main's 274 1-MiB
  `pwrite64`s take 34 ms in all (`strace -c --seccomp-bpf`).
- `perf --call-graph dwarf` of main's writer thread: `CachedFile::write_back` 11%, the flusher
  idle (0.4% CPU). `Cached::write_behind` woke the flusher alone, which writes nothing under
  32 MiB dirty or before a second: the store's waiting `write_back` of each range then wrote it
  on the writer's thread. Page faults 26%, zero-fill 16%, copies 19%: each write-back allocated
  a zeroed MiB twice.
- main read 87 MiB back from the device to frame what eviction took past the fixed 256 MiB cap;
  p 0.7 MiB.

## Results (medians, rounds 2-6)

| run | side | zvol write | zvol commit | RAM write | RAM commit | RAM durable |
|---|---|---|---|---|---|---|
| 1 | p | 336 µs | 59.0 ms | 513 µs | 36.7 ms | 1.57 ms |
| 1 | main | 922 µs | 391.4 ms | 862 µs | 200.6 ms | 0.70 ms |
| 1 | f | 989 µs | 141.5 ms | 754 µs | 144.8 ms | 0.66 ms |
| 2 | p | 335 µs | 51.4 ms | 287 µs | 56.0 ms | 0.77 ms |
| 2 | f | 486 µs | 110.8 ms | 405 µs | 78.5 ms | 0.36 ms |
| 2 | g | 534 µs | 113.5 ms | 371 µs | 67.5 ms | 0.34 ms |
| 3 | p | 304 µs | 51.8 ms | 319 µs | 77.9 ms | 0.85 ms |
| 3 | g | 498 µs | 152.2 ms | 431 µs | 108.5 ms | 0.39 ms |
| 3 | h | 494 µs | 45.7 ms | 376 µs | 33.1 ms | 0.44 ms |
| 4 | p | 294 µs | 48.7 ms | 311 µs | 94.0 ms | 0.84 ms |
| 4 | h | 467 µs | 47.8 ms | 368 µs | 32.9 ms | 0.42 ms |

The commit through the governor-sized cache is at or under the host page cache's; durable
flushes are about twice as fast. Burst writes stay 1.2x (RAM) to 1.6x (zvol) the host page
cache's: TASK-433.9. A fourth change, pages a write fills whole made unzeroed (`2ddfa1dcb`),
bought nothing measurable (RAM write 371 µs, zvol 572 µs) and was dropped.
