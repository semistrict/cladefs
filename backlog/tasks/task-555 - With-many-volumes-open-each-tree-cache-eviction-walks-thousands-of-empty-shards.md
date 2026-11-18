---
id: TASK-555
title: >-
  With many volumes open, each tree-cache eviction walks thousands of empty
  shards
status: To Do
assignee: []
created_date: '2026-11-18 15:25'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 946000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), fxfs-store review; verified against the code by independent reviewers. Severity: high.

**Where:** `crates/fxfs/src/object_store/tree_cache.rs`: `rg -n -F 'let first = self.turn.fetch_add(1, Ordering::Relaxed);' crates/fxfs/src/object_store/tree_cache.rs`

`TreeCacheBudget::enforce` evicts one item per pass. It picks a shard by walking every shard of every cache that shares the budget, round-robin, starting from a global `turn` that moves forward by only one per call. It stops at the first non-empty shard. To find each shard index it runs `find_map` over the caches list, so each step costs O(#caches), and it takes that shard's mutex. In the guest, `object_cache_items` is memory/16 KiB (fs.rs), which is above `SHARD_ABOVE` (24,560) for any guest of 512 MiB or more, so every volume's cache has 16 shards. doc-10's design keeps hundreds of volumes mounted, and the budget empties the idle caches on purpose. The non-empty shards are then the active cache's 16, in one contiguous run out of 16·N. So each eviction visits about 8·N empty shards, each costing about N/2 `find_map` steps plus a lock, unlock and an empty `pop_front`. This runs on the hot path: `lookup_or_reserve`'s placeholder insert and `invalidate`'s insert of every written record both call `enforce` whenever the budget is full.

**Trigger.** 1) A guest of 2 GiB (object_cache_items = 131,072; 16 shards per cache) has 256 repos mounted through fxfs-automount (doc-10's 'hundreds per sandbox'; mounts stay 600 s). 2) `git status` in one Linux-sized checkout lstats 91k files: each stat misses and reserves Object and DATA-attribute records (about 182k items, more than the budget). 3) After the idle volumes' few items are evicted, every further insert runs `enforce`: on average about 2,000 shard visits, each with a `find_map` over about 128 caches (about 260k steps) and about 2,000 mutex round trips. That is roughly 0.5 ms per stat-miss against about 100 ns, so the status takes minutes instead of under a second. With 32 volumes mounted the cost is already about 10-20 µs per insert.

**Why the tests stay green.** `test_shared_budget` uses two single-shard caches. The other budget tests use one cache. TASK-136.2's guest cases with 256 volumes measure memory per mount and mount/import latency, which only touch root directories and never fill the budget. No test or bench runs a workload that goes over `object_cache_items` with many volumes open.

**Backlog.** Related. TASK-136.2 (Done) built the shared budget and made no claim about its cost; TASK-26.18.7 (To Do) is generic per-operation TreeCache CPU, not the budget's walk over empty shards.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 With one budget (limit 2·SHARD_ABOVE) shared by 256 caches, one cache filled past the limit and then given 10,000 more inserts, the shards visited per eviction stay at most 2·SHARDS (32), whatever the number of caches.
- [ ] #2 A guest bench running git status on a large checkout with 256 volumes mounted finishes within 2x of the same status with 1 volume mounted.
- [ ] #3 A tree_cache.rs test with a cfg(test) counter of shards visited in enforce reproduces the 256-cache case and goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
