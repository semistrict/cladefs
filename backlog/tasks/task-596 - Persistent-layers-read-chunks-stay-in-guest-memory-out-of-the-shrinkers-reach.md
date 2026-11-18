---
id: TASK-596
title: >-
  Persistent layers' read chunks stay in guest memory, out of the shrinker's
  reach
status: To Do
assignee: []
created_date: '2026-11-18 15:31'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 987000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), fxfs-store review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/fxfs-posix/src/lib.rs`: `rg -n -F 'self.volume.tree().shrink_cache(count.try_into().unwrap_or(usize::MAX)) as u64' crates/fxfs-posix/src/lib.rs`

Every persistent layer in the guest (store layers, base layers through `BaseLayerObject`, the allocator's) is read through a `CachingObjectHandle`, because there is no layer pager and `as_slice` is None. Each 128 KiB chunk a read touches stays `Chunk::Present` until `purge()` or `clear()` runs. Nothing in cladefs calls `Layer::purge_cached_data`, `clear_cached_data` or `ObjectStore::clear_caches`. Upstream called them from fxfs-platform's FxVolume background task under memory pressure; cladefs dropped that crate and gave the guest no replacement. The guest's superblock shrinker hooks (`Fxfs::cached` / `free_cached`) shrink only TreeCache records. So the layer bytes a workload has read stay in the guest's kernel heap until a compaction replaces the layer or the volume closes. That is up to the whole metadata size of every open store. On top of it: the allocator's whole tree, which `load_extents` scans at every mount, and up to 16 MiB kept per compaction (`KeptWriter`). The guest cannot reclaim any of it. docs/code.md exempts only the in-memory skip-list layers ('bounded by its journal's size'), not these.

**Trigger.** 1) In a guest, mount a volume whose layers total X bytes (for example a monorepo's 1-2 GB, or several attached Linux checkouts at 18.9 MB of layers each). 2) Run a metadata scan: `find`, `du`, `git status`, rsync. Every layer chunk it seeks into is loaded and kept. 3) Memory pressure (or `echo 2 > drop_caches`) frees dentries, inodes and TreeCache records but none of the X bytes. With X above the guest's free memory, the guest OOMs or its allocations fail (EIO) while the cache holds the metadata. With many volumes attached within the 600 s automount window, the total is the sum of all their layers.

**Why the tests stay green.** TASK-26.19.10's `cache_shrinks` guest check counts only TreeCache records (FXFS_IOC_CACHED_OBJECTS). `test_clear_cached_data` calls `clear_cached_data` by hand. No test reads a layer larger than guest memory, or checks that layer chunk memory drops under pressure.

**Backlog.** RELATED: TASK-26.19.10 (Done) made the shrinker free TreeCache records and in-memory layers only; TASK-96.1 (Done) deleted fxfs-platform's FxVolume background task, upstream's only caller of purge_cached_data, with no replacement.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The guest's superblock shrinker (Fxfs::free_cached) purges persistent layers' read chunks, base layers and the allocator's included: after a full scan of an object store's tree, the shrinker path leaves no Present or Expired chunks, as a cached-bytes count on CachingObjectHandle shows.
- [ ] #2 In a guest whose volume's layers exceed a quarter of its memory, `find /mnt >/dev/null` then `echo 2 > drop_caches` brings the kernel's vmalloc and slab use back near its level before the scan.
- [ ] #3 Regression tests exist, an fxfs unit test for the shrinker path and a guest case for the drop_caches check, and go red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
