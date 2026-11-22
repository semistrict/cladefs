TASK-395.12: what a pass in full over a written cell with many fork points costs, by fork points.

Host: macOS, Apple M5 Pro, APFS, a file bucket. Release build of fxdax-gc and fxdax-gc-fill.

Bucket: `fxdax-gc-fill DIR 1 1 1 160 --vm 1024 P`: one VM-like cell, its branch of 160 segments of
1,024 extents each (16 to 112 bytes, so offsets and lengths vary), freeing none: 163,840 live
extents at the end (a 10 GiB disk in 64 KiB extents), forked to P points at segments evenly apart,
each a point of a cell of its own, with the checkpoint at its fork point that recording it wrote.
Then `fxdax-gc file://COPY/ --cell 1/1 --grace 0`, the VM's cell alone, on a fresh copy each time,
so every pass is in full (the cell never passed before), three runs each.

Checkpoints of the cell: P = 1: 1 of 748 KB; P = 10: 4.2 MB in all; P = 100: 38.5 MB in all,
8.9 to 752 KB each (about 4.6 bytes an extent, zstd).

| points | GET | ranged GET | LIST | bytes of checkpoints read | seconds (3 runs) |
|---|---|---|---|---|---|
| main ec149e6de | | | | | |
| 1 | 8 | 161 | 5 | 0.75 MB | 0.258, 0.255, 0.282 |
| 10 | 17 | 161 | 14 | 4.2 MB | 0.535, 0.573, 0.485 |
| 100 | 108 | 161 | 104 | 38.5 MB | 3.629, 3.751, 3.501 |
| TASK-395.18, .19 | | | | | |
| 1 | 8 | 161 | 4 | 0.75 MB | 0.233, 0.272, 0.223 |
| 10 | 17 | 161 | 4 | 4.2 MB | 0.479, 0.437, 0.406 |
| 100 | 108 | 161 | 4 | 38.5 MB | 2.959, 2.736, 2.970 |

## Reading

- Each fork point costs a pass in full one GET of its checkpoint (its bytes, ~4.6 B an extent of
  the state), and ~26 ms of one core for a 164k-extent state after TASK-395.18 (~33 ms before):
  decoding the checkpoint and folding every extent into what the pass keeps, O(points x extents).
  It cost a LIST too before TASK-395.19.
- sample(1) of the 100-point pass on main: ~60% of its CPU in parsing each extent's object key in
  `Live::behind_at` (`layout::kind`, `in_cell`, `hex`, memchr): TASK-395.18 parses each key once.
  After it, `Live::keep_point`'s map work and the decoding remain, both a cost an extent a point.
- So a written VM's pass costs about 1 + P/9 times its cost with one point: 1.8x at 10 points,
  12x at 100. TASK-395.12's cache would make it ~1x however many points (one object, the union,
  decoded once), for a protocol change (Bucket.tla, a new object under `<C>gc/`, its validation).
- Decision (hotbed question 174, TASK-395.12): not built now. Worth it once running VMs commonly keep
  dozens of live snapshots while written: at 30, a pass is ~4x.
