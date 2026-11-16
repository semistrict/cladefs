TASK-443.2: what an open, idle fork of a base holds of the process's heap, by the base's extents.

Host: macOS, Apple M5 Pro, 48 GiB, APFS; rustc 1.99.1; the sim profile (optimized as release is,
with debug assertions: fxdax's tests, with their failpoints, refuse a release build, TASK-320).

Bench: `remote::tests::bench_fork_extent_memory` (crates/fxdax/src/remote.rs), at the layer that
runs on macOS: fxdax's host stores (`ExtentStore` over `extent_store` and `extent_remote`) in one
process over a memory bucket, no daemon, no VM, no profile.

    cargo nextest run -p fxdax --cargo-profile sim --run-ignored only bench_fork_extent_memory --no-capture

(`FXDAX_BENCH_EXTENTS=64,1024` and `FXDAX_BENCH_FORKS=100` choose other sizes.) A base of E
extents of 4 KiB each, committed 1,024 at a time; 100 forks of it, opened one after another and
left idle, forked (a) from the store itself, live, whose writer writes a checkpoint for each fork,
as `extentd::tests::serves_hundreds_of_forks_of_a_base` makes its forks, and (b) from a point of
it, whose forks all start from the point's one state. Heap is counted exactly: the test binary's
global allocator (`crate::heap::Bounded`, `System` wrapped) counts live bytes; read before the
forks open and with 1, 10, 20, 50 and 100 open. bench.log has every line.

## Bytes held per open idle fork (marginal, forks 51 to 100), exact

| base extents | of a point: bytes a fork | B an extent | of a live store: bytes a fork | B an extent |
|---|---|---|---|---|
| 64 | 46,085 | 720 | 60,725 | 949 |
| 1,024 | 273,633 | 267 | 508,586 | 497 |
| 32,768 | 7,796,987 | 238 | 15,333,008 | 468 |
| 300,000 | 66,209,810 | 221 | 135,208,456 | 451 |

Total held with 100 forks open, measured: 32,768 extents 0.78 GB (point), 1.41 GB (live);
300,000 extents 6.62 GB (point), 12.42 GB (live). Every (E, F) asked for was reached.

- Forks of a point cost the same from the first to the hundredth (F = 1, 10, 100 within 0.1%).
- The first 16 forks of a live store read low, the store's map alone (7,792,850 a fork at 32,768):
  each one's decoded checkpoint takes the place of one the bucket's cache of decoded states held
  before the count began (`extent_remote::recovery::Decoded`, `KEEP` = 16). The arithmetic fits
  exactly: forks 11 to 20 average 10,809,157 = (6 x 7.79 MB + 4 x 15.33 MB) / 10. That cache
  itself holds 16 states whatever their size: 16 x 69 MB = 1.1 GB at 300,000 extents.
- Fixed, whatever the base: about 31 KB a fork (46 KB at 64 extents less its 15 KB map).

## By owner, per fork (each built alone under the same counter)

| | 64 | 1,024 | 32,768 | 300,000 | B an extent at 32,768 |
|---|---|---|---|---|---|
| store's map (`extent_store` `Extents`, a HashMap), table | 8,328 | 133,128 | 4,259,848 | 34,078,728 | 130 |
| store's map, key Strings (`Location::key`, 107 bytes each) | 6,848 | 109,568 | 3,506,176 | 32,100,000 | 107 |
| writer's map (`extent_remote` `Extents`, imbl OrdMap), nodes | 6,944 | 124,576 | 4,029,088 | 36,897,952 | 123 |
| writer's map, key Strings | 6,848 | 109,568 | 3,506,176 | 32,100,000 | 107 |

The store's map accounts for 99.6% of a point's fork at 32,768 (7,766,024 of 7,796,987). The
writer's map is held unshared by a fork of a live store (store's + writer's = 15,301,288 of
15,333,008) and shared, so free, by forks of a point. The table's bytes an extent swing with the
HashMap's load (130 at 32,768, 114 at 300,000). Keys here are 107 bytes; a real bucket's prefix
makes them longer.

## Closing

All 100 closed, the heap is 70 to 81 KB over what it was before them, once (runtime and bucket
state, not per fork). Opened and closed again, it is the same to the byte: asserted
(`assert_eq!(settled(), closed)`), at every size. No leak.

On main as of TASK-433, after this run, the check differed by 64 bytes now and then: three holders
outside the stores, each bounded, none a leak, which the benchmark now settles. The page cache's
list of files (TASK-433) names closed ones by `Weak`, their allocations kept until it is next pruned
(`settled()` prunes it, by `pages::shared().write_back_all()`); on macOS, each of the 32 shards of
the remotes' runtime's list of tasks allocates its pthread mutex, 64 bytes, the first time a task
lands in it (`every_task_shard_locked()` takes each once before the count); and each thread's list
of the shared locks it holds (`extent_store::shared`) allocated 64 bytes at its first lock, which
the remotes' runtime's threads now make as they start (`shared::prepare_thread`).

## Time to open a fork

Run alone (first run, 2026-11-15, the machine quiet): 3.4 to 5 ms at 64 and 1,024 extents; 5.7 to
8.7 ms (point) and 8.5 to 12 ms (live) at 32,768; 39 to 53 ms (point) and 69 to 93 ms (live) at
300,000. bench.log's times are of the rerun with the 20 and 50 samples, made while other agents'
test runs loaded every core (about 100 ms at any size): its bytes are exact, its times are not a
measurement.

## Is it counted

No. The governor (`extentd/governor.rs`, `memory.rs`) limits tenants' extent-cache pages and the
parts stores hold packed (`held_parts`); nothing of a store's maps. The anonymous-memory budget
(`extentd/process.rs`, `AnonBudget`) has parts for uploads, framings, fetches, rooms, held parts
and `OWN` = 64 MiB; these maps would be `OWN`'s. But its framings part adds 288 MiB for every store
open, so 400 idle forks raise the budget by 112.5 GiB and a sample never goes past it for this.

## Extrapolation (not measured: bytes a fork above x forks)

| base extents | 400 forks, point | 400 forks, live | 1,000 forks, point | 1,000 forks, live |
|---|---|---|---|---|
| 64 | 18 MB | 24 MB | 46 MB | 61 MB |
| 1,024 | 109 MB | 203 MB | 274 MB | 509 MB |
| 32,768 | 3.1 GB | 6.1 GB | 7.8 GB | 15.3 GB |
| 300,000 | 26.5 GB | 54.1 GB | 66.2 GB | 135.2 GB |

E.g. 32,768, point: 7,796,987 x 400 = 3,118,794,800. Linear in forks as measured to 100.

## Reading

- The review's claim holds, and its 150-200 B an extent was low: 221 to 238 B for the store's map
  alone, 451 to 468 B for a fork of a live store. Fix filed: TASK-453.
- Which a real host is depends on the extent count of a real base image, which nothing reports
  (`fxdaxctl stat` gives bytes): TASK-454. Not guessed here. Since TASK-454, `cladefsctl stat
  DIR` prints it, `own_extents` and `inherited_extents` (StatStore's fields of those names).
- Not measured here: the daemon on Linux (resident memory, glibc's overhead on these many small
  allocations, connections), which is TASK-443.2's first criterion.
