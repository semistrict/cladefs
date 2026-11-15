TASK-443.4: what cells whose branches are all deleted cost every later sweep, and what a pass over a base cloned N times lists and holds.

Host: this Mac (host.txt), a file bucket on APFS, release build, 16 passes at once, one run each.

## How

    export CARGO_TARGET_DIR=...   # the shared target
    R=docs/measurements/macos-gc-dead-cells-2026-11-15
    for n in 100 1000 10000; do
      FXDAX_GC_TENANTS=1 FXDAX_GC_CELLS=$n FXDAX_GC_SEGMENTS=4 FXDAX_GC_LARGEST=16 \
        FXDAX_GC_FILL="--deleted" FXDAX_GC_AT_ONCE=16 FXDAX_GC_SWEEPS=3 \
        scripts/gc-sweep.sh WORK/deleted-$n $R/deleted-$n
      FXDAX_GC_TENANTS=1 FXDAX_GC_CELLS=$n FXDAX_GC_SEGMENTS=4 FXDAX_GC_LARGEST=16 \
        FXDAX_GC_FILL="--clones 1024" FXDAX_GC_AT_ONCE=16 FXDAX_GC_SWEEPS=3 FXDAX_GC_CELL=1/1 \
        scripts/gc-sweep.sh WORK/clones-$n $R/clones-$n
    done
    # What is left, by kind and bytes (usage-clones-3.jsonl):
    fxdax-gc-fill D 1 3 4 16 --clones 1024; fxdax-gc file://D/ --grace 0   # three times
    fxdax-gc file://D/ --usage

- `--deleted`: N cells, each one root branch of 4 segments, checkpointed, deleted as a host
  deletes it (`branch::delete`, `cell::forget`).
- `--clones 1024`: a base (cell 1/1, 16 segments of 1,024 extents), a point of it (1/2), and N
  sandboxes (1/3 on), each a cell whose branch is a clone of the point (`branch::fork_point`),
  4 segments written, checkpointed, deleted. Then the pass over 1/1 alone, a process of its own:
  `unchanged` (the sweeps' options: skips), `full` (another grace: decides anew, as every pass does
  once a clone was made or retired since its last).
- Each directory: fill-this.txt, gc-sweep.md, gc-sweep-requests.md (by kind), gc-sweep-cell.md,
  the .prom of each sweep and pass. The sweeps' per-cell logs are left out.

## Third sweeps (every cell skipped, nothing deleted)

| bucket | cells | GET | update (PUT) | LIST | delimiter LIST | requests | a dead cell | objects left | seconds | ms a cell |
|---|---|---|---|---|---|---|---|---|---|---|
| deleted | 100 | 200 | 200 | 100 | 2 | 502 | 5.00 | 400 | 1.2 | 12 |
| deleted | 1,000 | 2,000 | 2,000 | 1,000 | 2 | 5,002 | 5.00 | 4,000 | 9.7 | 9.7 |
| deleted | 10,000 | 20,000 | 20,000 | 10,000 | 11 | 50,011 | 5.00 | 40,000 | 110.2 | 11 |
| clones | 102 | 204 | 204 | 102 | 2 | 512 | 5.00 | 525 | 1.0 | 10 |
| clones | 1,002 | 2,004 | 2,004 | 1,002 | 3 | 5,013 | 5.00 | 5,025 | 12.6 | 13 |
| clones | 10,002 | 20,004 | 20,004 | 10,002 | 12 | 50,022 | 5.00 | 50,025 | 188.9 | 19 |

Fit: requests a sweep = 5 x cells + 1 delimiter page per 1,000 cells + 1: per dead cell 2 GETs of
its lease (the taking's read, the release's), 2 conditional PUTs of it (taken, released), 1 LIST of
its prefix. Exact at every size. Peak memory of the sweep 12-22 MiB, not growing with cells.

## What a dead cell keeps (usage-clones-3.jsonl)

4 objects under its own prefix, 422 bytes: its cell object (46), its branch's record (67), its
head (73), its collector lease (236). A sandbox cloned from another cell's point keeps a fifth, its
retired mark under the base's cell (4 bytes). Nothing removes any of them. 10,000 deleted roots:
40,000 objects left; 10,000 sandboxes: 50,025 (5 a sandbox, and the base's and point's 25).

## The pass over the base's cell (1/1), after N clones made and deleted

| clones | objects listed | LIST pages (objects/1,000) | counted LISTs | skips unchanged | unchanged: s, peak MiB | full: requests, s, peak MiB |
|---|---|---|---|---|---|---|
| 100 | 122 | 1 | 1 | yes | 0.018, 10 | 33, 0.054, 31 |
| 1,000 | 1,022 | 2 | 1 | yes | 0.078, 10.5 | 33, 0.222, 32 |
| 10,000 | 10,022 | 11 | 1 | yes | 0.261, 19 | 33, 0.283, 39 |

- The cell holds a mark a clone ever retired (10,000 of its 10,022 objects), and both passes list
  them all: the one that skips lists the whole prefix to make its digest before deciding. The
  counter says 1 LIST (2 GET, 2 PUT, 1 LIST: 5 requests); a store of 1,000 keys a page serves it in
  objects/1,000 requests.
- Memory: +9 MiB over 9,000 marks more, about 1 KiB a mark listed, in either pass.
- It skips only if no clone was made or retired since its last pass: a clone's reference and its
  retirement's mark each change the listing (the fill tool's test asserts it: a sandbox made and
  deleted after puts the base's pass in full for two sweeps). A base cloned every sweep interval
  never skips; in full it also reads its checkpoint and its 16 segments' trailers each time.

## Also seen

- A dead cell is passed over in full twice before it skips: the first pass's deletes change the
  listing its record went by, and the second retires the reference again (10,000 creates of marks
  already there in sweep 2).
- Seconds a cell grow with the bucket here (10 to 19 ms a cell at 16 at once): the file bucket's
  fsyncs and APFS directories, not the collector. Requests are the measurement; times on a file
  bucket say little of S3.

## Not reached

- 100,000 cells: the fill of 10,000 sandboxes took 236 s and a sweep 189-281 s; ten times that is
  past the 5 minutes a run. The fit is exact over two orders of magnitude and has no term that
  could bend.
- A real bucket's latency and its pages: file buckets only (no ubu1, no cloud in this task).
