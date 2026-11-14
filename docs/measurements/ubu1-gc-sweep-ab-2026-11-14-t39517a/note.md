TASK-395.17: 4 passes at once, first sweeps, 4 rounds interleaved (A B, B A), c9b5ba549 (old) against main ec149e6de (new).

Host: ubu1.

## Reading (TASK-395.17)

Both sides in one `fxjob --bench` job on 8 CPUs, each built in its own target and swept over a
bucket of its own fill (same shape: 1.0M objects in 10k cells), by scripts/gc-sweep.sh's sides,
first sweeps only, the sides' order turned each round (old new, new old, ...).

| side | at once | runs | median s | least s | most s |
|---|---|---|---|---|---|
| old c9b5ba549 | 4 | 4 | 539.5 | 505.1 | 600.5 |
| new ec149e6de | 4 | 4 | 496.4 | 474.2 | 524.7 |

- Main is not slower at 4 passes at once: its median is 8% under the old one's, the ranges
  overlapping. The 320 s of ubu1-gc-sweep-2026-11-13T2259Z-50e7eb9e (old) and 559 s of
  ubu1-gc-sweep-2026-11-14T0233Z-e344b176 (new) were one run each: within or beyond this spread.
- The sweep is bound by the scratch disk, not CPU: top showed fxdax-gc at ~46% of a core, ~11%
  iowait, ext4's jbd2 and the zvol's threads busy (ubu1's /mnt/fxdax-scratch is ext4 on a ZFS
  zvol), the file bucket fsyncing each lease update and staged file. A run's time swings with the
  host's disk, which is why single runs disagreed by 1.75x.
- No cause to fix: neither the worker count, the 64 lock stripes nor blocking-pool hand-offs show.
- Per-cell logs (sweep-*.log) and remote.log left out; the run's directory on ubu1 is in run.txt.
