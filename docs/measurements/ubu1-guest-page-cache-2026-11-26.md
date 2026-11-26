# What a build guest's page cache holds, and what dropping it gives the host (board 13454), 2026-11-26

scripts/measure-guest-page-cache.sh in a hotbed build VM on ubu1: 8 vCPUs, 18.5 GiB guest
(MemTotal 18917 MiB), guest kernel 6.18.54, root `cladefs:rootfs` with a DAX window,
`page_reporting_order` 9, hotbed's dirty limits (`dirty_background_bytes` 64 MiB,
`dirty_expire_centisecs` 500, hotbed d0e78922). It built `cladefs` (tree f7db7dfbd, before main
split the crate) into a new target directory with `CARGO_BUILD_JOBS=4`, so every output was a file
created in this mount, then dropped the cache in steps, holding 40 s after each. A loop on the host
read the Firecracker process's `/proc/PID/status` and its cgroup every 2 s. One run, not a
`--bench` measurement: ubu1 was full of other sessions' VMs, so the rebuild times are indicative
only. Raw logs: [guest.log](ubu1-guest-page-cache-2026-11-26/guest.log) (phase times are epoch
seconds, the same clock as the host's), [host.tsv](ubu1-guest-page-cache-2026-11-26/host.tsv).

| phase | guest Cached | guest MemFree | host VM RSS | host VM swap | rebuild |
| --- | --- | --- | --- | --- | --- |
| boot | 20 MiB | 18217 MiB | 464 MiB | 0 | |
| built (242.7 s) | 7928 MiB | 9474 MiB | 9295 MiB | 1590 MiB | |
| warm rebuild | 7904 MiB | 9941 MiB | 11327 MiB | 1497 MiB | 33.3 s |
| sync, then 40 s idle | 7904 MiB | 9495 MiB | 11060 MiB | 1369 MiB | |
| fadvise DONTNEED on the outputs, +40 s | 27 MiB | 17418 MiB | 5022 MiB | 1191 MiB | |
| rebuild after fadvise | 6457 MiB | 10461 MiB | 10246 MiB | 373 MiB | 28.4 s |
| sync, drop_caches 3, +40 s | 20 MiB | 17887 MiB | 4928 MiB | 267 MiB | |
| compact_memory, +40 s | 20 MiB | 17883 MiB | 4017 MiB | 259 MiB | |
| rebuild after drop | 6451 MiB | 10894 MiB | 10186 MiB | 309 MiB | 27.2 s |

## The cache is the build's outputs, clean

- After the build, `Cached` was 7.9 GiB against 209 MiB of anon. `Dirty` was 0.1 MiB: hotbed's
  dirty limits hold, and none of the 7.9 GiB is waiting on writeback.
- Every output was a page-cache inode (0 of 7041 files DAX), and `cachestat` found them all fully
  cached. A walk of the whole root after `sync` found nothing else with a MiB cached but
  `.git/index` (1.2 MiB). So the guest's file cache was the build's own outputs, clean.
- The 1133 inodes in `dax_unsealed_inodes` after the build were 0 after `sync`: sealed.
- On the host the VM held 9.3 GiB resident and 1.6 GiB in swap once the build was done, peaking at
  11.7 GiB resident and 2.6 GiB swapped.

## Nothing comes back on its own; dropping it comes back within seconds

- 40 s idle after `sync`: guest free memory and host RSS flat.
- `POSIX_FADV_DONTNEED` on the outputs alone freed 7.9 GiB in the guest, and free page reporting
  handed 6.0 GiB of it to the host within about 15 s (RSS 11060 to 5022 MiB) at order 9, with no
  compaction.
- `drop_caches 3` after the next rebuild: 4.4 GiB back to the host within about 20 s.
- `compact_memory` then gave 0.9 GiB more (RSS 4883 to 4017 MiB).

## Order 9 leaves about 2 GiB of free guest memory on the host

After compaction the guest had 17.9 GiB free, yet the VM still held 4.0 GiB resident. `buddyinfo`
put 2.25 GiB of the free memory in blocks of order 0 to 8, which reporting at order 9 never hands
over (4.1 GiB before compaction). The rest is the guest's 0.6 GiB in use and Firecracker's own.

## Dropping costs the next build step nothing measurable

The rebuild (touch `crates/cladefs/src/lib.rs`, build: recompile and relink, reading every
dependency's `.rlib`) took 33.3 s warm, 28.4 s after fadvise, 27.2 s after `drop_caches 3`.
Unisolated, so within noise; no sign that reading the outputs back costs anything.

## Most outputs are never evicted: the inode LRU walk livelocks on DAX inodes (board 13582)

After `drop_caches 3` the walk read the outputs in again: 1 of 7041 came back DAX, and the rebuild
after it filled 6.45 GiB of page cache, as the one after fadvise did. doc-13 says an inode read in
once its writes are sealed is DAX (`cladefs_dax_set_inode`); the outputs were never read in again,
because their inodes were never evicted.

scripts/probe-created-promotion.sh found why, on a smaller build (`storage_device`, 738 outputs),
with kprobes in the guest:

- A file made alone, in any of the ways a build makes one (written, hard-linked, renamed, its
  times set, grown by truncate, O_TMPFILE then linked, written through a shared mapping, 64 B,
  3000 B), synced and dropped, reads in again as DAX
  ([probe-ways-of-making-a-file.log](ubu1-guest-page-cache-2026-11-26/probe-ways-of-making-a-file.log)).
- Of the build's outputs, 122 came back DAX and 616 stayed in the page cache (264 and 407 in
  another run). Five more drops, or an fsync of each, changed nothing. The build reclaimed nothing
  (`pgscan` 0, `memory.high` max), and no output held a shadow entry.
- At its final `iput` during the drop, every output, stuck or not, was clean (`i_state` 0),
  unreferenced and held no pages; `list_lru_add` put each on the build's memory cgroup's inode
  list, and `prune_icache_sb` walked that list with `nr_to_scan` up to 1025 a call
  ([probe-shrinker-accounting.log](ubu1-guest-page-cache-2026-11-26/probe-shrinker-accounting.log)).
- `inode_lru_isolate` never met the stuck ones. Of 11,914 calls, 7,629 and 2,866 met two inodes,
  each `i_state 0 count 0 nrpages 1` with a value entry as its xarray's head: a DAX inode whose one
  DAX entry is at index 0 ([probe-lru-walk.log](ubu1-guest-page-cache-2026-11-26/probe-lru-walk.log)).

In the guest's kernel (Amazon Linux 6.18.54, and upstream 7.3 alike), `mapping_shrinkable()` lets
such an inode onto the list, a single value entry at the head being taken for a shadow entry;
`inode_lru_isolate` finds the mapping not empty and calls `invalidate_mapping_pages`, which leaves
DAX entries alone (`clear_shadow_entries`: "for DAX we do nothing"), and returns `LRU_RETRY`; the
walk restarts at the same inode, a step of its budget spent each time. Every walk of that list
stops there, and no inode behind it is evicted, by `drop_caches` or by memory pressure. Any small
file a build reads through the window (a crate's sources, a `Cargo.toml`) is such an inode.

## What this means for the plan (doc-30)

- The cache is the build's clean outputs, and dropping it hands the host the memory within
  seconds: freeing guest RAM needs no change to writeback or to free page reporting's order,
  though order 9 strands about 2 GiB.
- Promotion to DAX by eviction, which doc-30 builds on, does not happen for most outputs until the
  inode LRU stops livelocking on DAX inodes (board 13582). Until then any drop, hotbed's proactive
  reclaim included, is refilled by the next build step through virtio.

## Caveats

- One run on a loaded host; the rebuild times are not a measurement.
- The run's `cachestat` sums counted each hard link of cargo's outputs (`deps/` and `debug/`):
  they read 11.2 GiB cached against `Cached` 7.9 GiB. The script counts each inode once since.

## On the new kernel (guest-kernel-2d730c0a, 6.18.62 with Linux's fix), the same afternoon

hotbed's VMs moved to the new kernel at about 13:15 (board 13582, 13454). The same measurement,
building `cladefsd` (main had split `cladefs`; not the same build, so its times don't compare),
in an 18.5 GiB ubu1 guest ([guest-new-kernel.log](ubu1-guest-page-cache-2026-11-26/guest-new-kernel.log)):

| phase | guest Cached | outputs DAX |
| --- | --- | --- |
| built (289.8 s) | 1348 MiB | 4130 of 7350 |
| warm rebuild | 417 MiB | 7325 of 7350 |
| fadvise DONTNEED on the outputs | 20 MiB | 7350 of 7350 |

The build itself ran into ubu1's memory limit, and hotbed's worker reclaimed this guest's page cache
five times in three minutes (`reclaimed guest page cache under host memory pressure`): on this
kernel each drop evicted the outputs' inodes, and the build read them back through the window.
On the old one the same drops left them page-cache inodes, and the guest held 7.9 GiB. cladefsd
then let the VM go and killed it in the rebuild after the fadvise (`closed its connection,
reaching pages still`), as it had killed seven old-kernel VMs on ubu1 earlier that day (board
13944), so the run has no drop_caches phases.

scripts/sample-vm-memory.py sampled every hotbed VM on ubu1 and asahimbp every 5 minutes for two
hours after the switch ([fleet-samples.jsonl](ubu1-guest-page-cache-2026-11-26/fleet-samples.jsonl)):
the guest cgroup of a VM running a command held a median 0.4 GiB of file cache on ubu1 (51 samples)
and 0.8 GiB on asahimbp (20), but up to 9.6 and 12.2 GiB during big builds, until hotbed's reclaim,
which runs only once the host is short (5% PSI, cladefsd pressure, 80% of its limit), dropped it.
The kernel's fix makes a drop stick; when to drop, before the host is short, is still hotbed's
guestd's to decide (doc-30 §2). Too few old-kernel VMs ran in the window to compare.

## Free page reporting's order: 9 against 7 (doc-30 §3)

scripts/measure-page-reporting.sh, six runs on ubu1 on the new kernel, interleaved 9, 7, 7, 9, 7, 9,
each in a VM of its own (18.5 GiB guest, 8 vCPUs): set the order, build `cladefsd`, drop every
cache, hold 60 s, compact, hold 60 s; every VM's host RSS sampled each 2 s
([page-reporting-runs.log](ubu1-guest-page-cache-2026-11-26/page-reporting-runs.log),
[page-reporting-host.tsv](ubu1-guest-page-cache-2026-11-26/page-reporting-host.tsv)).

| order | builds | mean | host RSS + swap held at the end | free below the order, in the guest |
| --- | --- | --- | --- | --- |
| 9 (2 MiB, the default) | 211, 228, 214 s | 218 s | 4.81, 3.35, 2.11 GiB (mean 3.42) | 2724, 1485, 2343 MiB |
| 7 (512 KiB) | 269, 267, 319 s | 285 s | 2.98, 2.89, 2.69 GiB (mean 2.85) | 961, 749, 680 MiB |

Order 7 handed the host about 0.6 GiB more of a drained guest on average, and every build at it
was slower, by 31% on average: reporting free blocks of 512 KiB as the build frees them, the guest
faults each in again from the host as it reuses it. What the guest holds free below the order
overstates what the host keeps: a block reported once and then partly reused costs the host only
the pages touched since. Order 9 stays.
