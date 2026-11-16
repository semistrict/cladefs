TASK-453: what an open, idle fork of a base holds of the process's heap, before and after the forks
share what they inherit. Beside TASK-443.2's (../macos-fork-extent-memory-2026-11-15/note.md).

Host: macOS, Apple M5 Pro, 48 GiB; the sim profile. Bench: `remote::tests::bench_fork_extent_memory`
(crates/cladefs/src/remote.rs), heap counted exactly by the test binary's counting allocator:

    CLADEFS_BENCH_PARENTS=live CLADEFS_BENCH_EXTENTS=32768 CLADEFS_BENCH_FORKS=100 \
        cargo nextest run -p cladefs --cargo-profile sim --run-ignored only \
        -E 'test(bench_fork_extent_memory)' --no-capture

Before: main at 21f042594 (the same benchmark, its binary run alone). After: this branch. Each
(extents, forks, parent) run in a process of its own. Logs: before-*.log, after-*.log.

## Bytes of heap per open idle fork, marginal (forks 2-10, or 51-100)

| base extents | forks | of a live store, before | after | of a point, before | after |
|---|---|---|---|---|---|
| 1,024 | 10 | 300,233 | 31,795 | 274,186 | 31,537 |
| 1,024 | 100 | 509,188 | 36,049 | 273,633 (TASK-443.2) | 31,927 |
| 32,768 | 10 | 8,635,139 | 31,984 | 7,797,482 | 32,005 |
| 32,768 | 100 | 15,333,671 | 35,993 | 7,797,783 | 31,878 |
| 300,000 | 10 | 73,876,691 | 31,848 | 66,210,184 | 31,697 |
| 300,000 | 100 | 135,209,127 | 36,140 | 66,210,495 | 31,829 |

Total with 100 forks open, 300,000 extents: 12.42 GB (live) and 6.62 GB (point) before; 3.41 MB and
3.19 MB after. A fork no longer holds anything per extent of its base: 27-36 KB each at every size
(live forks grow from 28 to 36 KB over the first 50 as the base's writer's list of forks and the
decoded states' registry grow). The benchmark now fails past 64 KiB a fork (`IDLE_FORK_BYTES`).

1,024 x 100 of a point, before: the benchmark's closing check (TASK-471) failed before its point
half every run, so TASK-443.2's figure for that size is given.

## What is held once a base, and how it is counted

- The decoded state the forks share: one map of the base's extents for the bucket
  (`Decoded::held`: 1,024 / 32,768 / 300,000 entries, asserted). At 300,000 it is two states, a
  checkpoint of the base read earlier and the fork point's state rebased onto it, holding only
  what changed: 300,000 entries together.
- A map of the base's extents built sharing nothing, as a base's own writer's is: 234,144 B at
  1,024, 7,535,264 at 32,768, 68,997,952 at 300,000 (229-230 B an extent, 107 of it the key
  string, TASK-472), under `extent_remote::recovery::ENTRY_BYTES` = 512 (asserted).
- A fork's writes, committed and uploaded, by what closing it frees more than closing an idle
  fork: 556-557 B a block at 1,024 writes, 460 at 4,096 (32,768-extent base), 245-246 at 4,096
  (300,000-extent base), under `extent_store::OWN_ENTRY_BYTES` = 1,024 (asserted). The extent
  daemon's anonymous-memory budget counts both (`AnonBudget::extents`).

## The closing check (TASK-471)

Opened and closed again, the heap was off by 213-2,048 B whenever a point's half ran alone in a
fresh process (6 of 6), and once in 6 live runs (32,768 x 100: 213 B). With both halves in one
process, as main runs them, it passed 3 of 3 at 1,024 x 10: first-time allocations of a cold
process, the class TASK-471 is about, not held per fork. The benchmark checks it last now, after
its other numbers and assertions.
