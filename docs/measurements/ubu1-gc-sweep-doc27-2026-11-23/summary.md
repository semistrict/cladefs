# cladefs-gc's sweeps before and after doc-27 (board 6222)

ubu1, 2026-11-23, `scripts/gc-sweep-host.sh` with two sides:
- before = `e7a694cb1`: main before doc-27 step 1. The collector compacts sparse objects into moves (`compact_below` 0.5, grace 60 s) and unwraps cells' KEKs to do it.
- after = `63dfb2e03`: main with doc-27 steps 1 to 4. Writers relocate, and the collector is keyless and compacts nothing.

The bucket is `cladefs-gc-fill`'s, each side filling its own with its own build: 20 tenants of 100 cells, 212,855 objects, 151 MB. Each cell's segments hold four extents of 1 KiB, and the next segment frees three of them, every other one all four. Three sweeps at 1 and at 16 passes at once. Tables: `gc-sweep.md`, `gc-sweep-requests.md`.

## Requests and objects

| | sweep 1 requests | sweep 2 | sweep 3 | ranged GETs, sweep 1 | creates, sweep 1 | objects left |
|---|---|---|---|---|---|---|
| before | 371,302 | 367,302 to 369,138 | 4,021 to 338,603 | 96,427 | 96,427 | 214,874, falling to 120,447 as its grace passed |
| after | 30,021 | 26,021 | 4,021 | 0 | 2,000 | 102,447 |

- **About 12x fewer requests a full sweep.** Before, every pass read each candidate's trailer (ranged GETs) to measure its density, then wrote a moves object for each object it compacted (creates). After, a pass reads heads and checkpoints, lists, deletes, and writes its lease: 2,000 creates are the cells' leases.
- **The second sweep goes in full on both sides.** A pass that deleted records its listing from before its deletes. The third skips every unchanged cell: 4,021 requests, 2 a cell, plus the listing of the tenants.
- **Before left the bucket larger.** Compaction added moves objects, 214,874 objects after the first sweep against 212,855 filled. What they replaced waited out the grace period, so the 1-at-once side's later sweeps deleted them; the 16-at-once side's sweeps came within the 60 s and kept them. After deletes what no state reads at once: 102,447 objects left, every cell's sparse segments kept. The fill has no writers to relocate out of them, and a real writer does that as it writes (the soak measures it).

## Time and memory

| | sweep 1 s, 1 at once | sweep 1 s, 16 at once | peak MiB, 1 / 16 at once |
|---|---|---|---|
| before | 205.1 | 30.7 | 20 / 55 |
| after | 45.6 | 9.9 | 9 / 30 |

hotbed flagged the measurement as having lost its isolation: another workspace wrote 280 MB in 2.8 s during it (`remote.log`). The seconds may carry that noise. The requests and objects are counts, which it doesn't change.

No regression to file.
