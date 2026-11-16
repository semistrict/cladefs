TASK-455: TASK-443.4's measurement (docs/measurements/macos-gc-dead-cells-2026-11-15) redone with
dead cells removed whole, clones' references retired unmarked, passes that look before taking the
lease, and LISTs counted by their pages.

Host: this Mac (host.txt), a file bucket on APFS, release build, 16 passes at once, one run each.
Requests are the measurement; seconds on a file bucket say little of S3.

## How

The same commands as the measurement before, the variables renamed fxdax -> cladefs:

    export CARGO_TARGET_DIR=...   # the shared target
    R=docs/measurements/macos-gc-dead-cells-removed-2026-11-15
    for n in 100 1000 10000; do
      CLADEFS_GC_TENANTS=1 CLADEFS_GC_CELLS=$n CLADEFS_GC_SEGMENTS=4 CLADEFS_GC_LARGEST=16 \
        CLADEFS_GC_FILL="--deleted" CLADEFS_GC_AT_ONCE=16 CLADEFS_GC_SWEEPS=3 \
        scripts/gc-sweep.sh WORK/deleted-$n $R/deleted-$n
      CLADEFS_GC_TENANTS=1 CLADEFS_GC_CELLS=$n CLADEFS_GC_SEGMENTS=4 CLADEFS_GC_LARGEST=16 \
        CLADEFS_GC_FILL="--clones 1024" CLADEFS_GC_AT_ONCE=16 CLADEFS_GC_SWEEPS=3 CLADEFS_GC_CELL=1/1 \
        scripts/gc-sweep.sh WORK/clones-$n $R/clones-$n
    done
    # What is left, by kind and bytes (usage-clones-3.jsonl):
    cladefs-gc-fill D 1 3 4 16 --clones 1024; cladefs-gc file://D/ --grace 0   # three times
    cladefs-gc file://D/ --usage

cladefs-gc-fill now gives its roots' and sandboxes' branches IDs from an allocator, as hosts do
(the base and the point keep ID 1), so its buckets also hold the allocator's blocks of IDs
(`objectid/`, one per filling thread that took one: 2 to 16). The sweeps' per-cell logs are left
out.

## Third sweeps, before and after

| bucket | cells | requests before | requests after | objects left before | objects left after (of which `objectid/`) |
|---|---|---|---|---|---|
| deleted | 100 | 502 | 1 | 400 | 7 (7) |
| deleted | 1,000 | 5,002 | 1 | 4,000 | 12 (12) |
| deleted | 10,000 | 50,011 | 1 | 40,000 | 5 (5) |
| clones | 102 | 512 | 6 | 525 | 26 (3) |
| clones | 1,002 | 5,013 | 6 | 5,025 | 28 (5) |
| clones | 10,002 | 50,022 | 6 | 50,025 | 26 (3) |

- After: a dead cell's first pass in full removes it whole (its record, head, object and lease;
  a sandbox's reference retired unmarked), and no later sweep lists it. The third sweep over the
  deleted roots is one delimiter LIST of `tenants/`, which finds none; over the sandboxes, a look at
  each of the base's and the point's cells (the lease's GET and a LIST, 2 requests each) and the
  two delimiter LISTs. Nothing grows with the cells deleted: 0 requests a dead cell a sweep, against
  5. What is left is the base's and point's cells (23 objects) and the allocator's blocks.
- The first sweep pays once: 7 GETs, 2 LISTs, 1 create, 2 updates, 3 delete batches a dead root
  (the take, its records and head read, its ID's block checked, its garbage, then its head and
  object, its records, its lease deleted); a sandbox 4 delete batches, its reference's among them.
- Seconds a sweep: the first as before (1.8 / 19.6 / 188.5 s deleted, 2.0 / 19.8 / 189.3 s clones:
  the file bucket's fsyncs); the third 0.0 s at every size (110.2 and 188.9 s before at 10,000).

## The pass over the base's cell (1/1), after N clones made and deleted

| clones | objects listed before | after | LIST pages before | after | unchanged: requests, peak MiB before | after | full: requests, peak MiB before | after |
|---|---|---|---|---|---|---|---|---|
| 100 | 122 | 22 | 1 | 1 | 5, 10 | 2, 9 | 33, 31 | 33, 31 |
| 1,000 | 1,022 | 22 | 2 | 1 | 5, 10.5 | 2, 9 | 33, 32 | 33, 31 |
| 10,000 | 10,022 | 22 | 11 | 1 | 5, 19 | 2, 9 | 33, 39 | 33, 32 |

The base's cell holds the point's reference and its own objects, nothing of the clones gone
(usage-clones-3.jsonl: 22 objects, no `retired`; before 25, 3 of them marks). Its pass skips once a
sweep went by with no clone made or retired, by a look (2 requests); a clone made or retired still
sends it in full, as its reference changes the listing (by design, TASK-395.1).

## Also

- A dead cell is passed in full once, not twice: removed by it; or, if kept (a writable fork's
  child, a clone never made, a named ID), recorded as what its deletes left, so the next skips it.
- A LIST of many pages is counted as its pages (sweep::Counted, Store): no multi-page listing is
  left in these runs' third sweeps to show it; the unit tests pin it.

## Not reached

As before: 100,000 cells, a real bucket.
