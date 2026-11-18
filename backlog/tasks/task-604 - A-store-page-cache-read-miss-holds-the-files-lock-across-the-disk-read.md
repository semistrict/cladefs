---
id: TASK-604
title: A store page cache read miss holds the file's lock across the disk read
status: To Do
assignee: []
created_date: '2026-11-18 15:32'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 995000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), local-io review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/local_cell/src/pages.rs`: `rg -n -F 'shared.file.read_at(&mut buf[to], at)?;' crates/local_cell/src/pages.rs`

`Cached::read_at` takes the file's `pages` mutex (`shared.lock()`) and keeps it for the whole call. That includes the miss path, which reads every uncached run straight from the O_DIRECT file. `write_runs` drops the same lock around its disk write, but reads never do. A cold read can be large: an upload reads a whole 32 MiB framed segment or own extent through `read_object`, then `read_cached`, then `read_at`. While that read runs, everything else that needs this file's lock waits for the disk. That includes the flusher of the process-wide cache: every `flush_pass` builds `files_by(|file| file.dirtied())`, and `Shared::dirtied` locks every file's pages. Also blocked are the governor's `evict_to` (through `evict_clean`), `Cached::size`, the end of this file's own write-back (`InFlight::end` re-locks), and guest WRITEs and READs on the same file. One example is the open segment's raw file, where a cold guest READ blocks the WRITEs going to it. `Cached::write_at` has the same pattern: it reads a partial page from disk while holding the lock.

**Trigger.** 1) The daemon serves stores A and B. B's bucket is slow, so its write-ahead log builds a backlog of framed segments. They are the least recently used files, so eviction (`evict_to(most)` / the governor) drops their clean pages. 2) B's uploader calls `Writer::create`, then `Body::read`, then `Closed::bytes`, then `read_object`. That calls `Cached::read_cached`/`read_at`, which misses on every page and reads about 32 MiB from disk at pages.rs:858 while holding the segment file's `pages` mutex. This takes tens of ms on NVMe and 100+ ms on a network disk, and repeats for every segment B uploads. 3) Meanwhile the flusher's pass (`flush_pass`, pages.rs:418) calls `file.dirtied()` on B's segment file and blocks until the read ends. No file of any store is written back during that time, including begun ranges and expired pages. 4) A guest burst on store A pushes dirty bytes past `hard`. `throttle()` waits for `dirty <= hard`, which only the flusher's write-back brings about, so A's WRITEs stall for the whole duration of B's disk read, once per segment.

**Why the tests stay green.** The pages tests are single-threaded, or they check only contents and usage counts. No test runs a read miss at the same time as the flusher, as a writer on another file, or as a writer on the same file. MemDir caches have no flusher at all (`PageCache::without_flusher`). The latency appears only under a real disk with an upload backlog, which no case measures.

**Backlog.** No related task covers it: TASK-433.9 (Done) dropped the pages lock around write_runs' disk write only; TASK-433.11 (To Do) is write-back latency at one write in flight a file; TASK-517 (To Do) is a persistent write-back error stalling writers.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 While a cold read_at of one file is parked inside its disk read, the flusher writes back another file dirtied past `hard`, and that file's writer returns within a bound rather than waiting for the read.
- [ ] #2 While one file's read miss is in flight, a write_at to a different page of the same file, Cached::size and evict_clean complete without waiting for it; write_at's partial-page read does not hold the lock across the disk either.
- [ ] #3 A regression test in local_cell's pages.rs uses a PageCache::new (with flusher) over two files, one whose read blocks on a barrier, asserts both of the above, and goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
