---
id: TASK-566
title: 'Once the part cache drops a hot object''s tail, it never keeps its parts again'
status: To Do
assignee: []
created_date: '2026-11-18 15:27'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 957000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), bucket-read review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/bucket_cell/src/fetch.rs`: `rg -n -F 'if let Some(Err(err)) = self.cached(|cache| cache.keep_part(key, part, bytes)) {' crates/bucket_cell/src/fetch.rs`

`Cache::keep_part` keeps a part only if the cache's index holds its object with its tail. Otherwise it returns `Ok(())` without a word (cache.rs: `let Some(&stored) = stored else { return Ok(()) };`, and it also requires `object.readable()`). A tail leaves the cache with its object's last part (`make_room`), or through `forget` (CRC failure, file gone). The fetcher puts a tail back into the cache in one place only, `read_tail` (`self.keep_tail(key, &tail)`), and that runs only when the host's in-memory `Tails` misses. `Tails` holds 64 MiB of tails and is touched by every `Fetcher::read`, so a hot object's tail never misses. RECENT hits touch neither the cache's parts nor its tails, so an object read mostly from memory ages out of the cache's LRU while its tail stays in memory. From then on, every part of it that misses RECENT (about 32 parts host-wide, since each counts stored plus unpacked) goes to the bucket in `Fetching::part`. `kept()` finds no object, `fetch` GETs (~30 ms instead of an SSD read), and `keep` silently keeps nothing. Read-ahead behaves the same, since `holds` is false. The object never recovers until its tail leaves memory or the daemon restarts. This breaks AGENTS.md's 'no silent fallback to a costlier path: the bucket where a local file was meant to serve'.

**Trigger.** 1. A host's part cache drops below its floor during a bucket outage, for example in the bucket soak where the write-ahead log grows on the same disk. `make_room` on each keep evicts every object's parts and then their tails (TASK-443.8's behaviour). 2. Meanwhile guests keep reading a base whose tails stay in `Memory::tails`. 3. The bucket recovers, the log drains, and the disk has room again. 4. Each read of that base's parts that misses RECENT now does `kept()` -> None, then a bucket GET, then `keep_part`, which drops it because there is no object in the index. That repeats on every miss, indefinitely. The same thing happens without a fault when a hot object served from RECENT has its cached parts and then its tail evicted by other objects' fetches under capacity pressure, or when `keep_tail` once found no room (`!fits`).

**Why the tests stay green.** The fetch.rs cache tests (`reads_keep_what_they_fetch`, `fetchers_sharing_a_cache_read_the_tails_kept_there`, `fetchers_of_a_host_share_what_they_hold_in_memory`) read an object once, or remove the cache file and then read only what RECENT still holds. None evicts an object from the cache while its tail stays in memory and then reads a part RECENT no longer holds to check that the part is kept again. The cache.rs tests call keep_part only with the tail kept.

**Backlog.** Related: TASK-443.8 (To Do) covers the part cache keeping nothing while under its floor, not parts staying uncached afterwards. TASK-165.4 (Done) set the rule 'a part only with its tail' without ever putting tails back. TASK-528 (To Do) is local cache I/O errors that turn into GETs.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The part cache can drop an object's tail through floor eviction, forget, or finding no space at the first tail keep, while the tail stays in the fetchers' memory. A part of that object read after it has left RECENT is then kept in the part cache again. Measured: each part costs at most one bucket GET, and its next read makes no bucket request.
- [ ] #2 A regression test in crates/bucket_cell/src/fetch.rs reads part 0 of KEY with a part cache, calls cache.forget(KEY), reads part 1, pushes part 1 out of RECENT and reads it again. It asserts cache.holds(KEY, 1) and that the second read made no bucket request, and it goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
