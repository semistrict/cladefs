# An image's device reads its sealed extents through the extent cache

**Tried:** 2026-11-22, board 8260, worktree `t3/unify-host-storage-opencelldevice` (import-bench
`IMPORT_BENCH_IMAGE`, binaries B and C against D on ubu1's bench CPUs, hotbed run `ja2kvleix5`).
**Idea:** `OpenCellDevice`, the device every image this process mounts is on, would take
`OpenCell::read` whole, as the extent daemon's `READ` does: sealed extents through the extent cache
(`crate::cache`), so that a host's FUSE mount and its VMs share one copy of a base's pages.
**Result:** reading an image's 1 GiB file back, its pages cold: 1.3 GiB/s through the extent
cache, 2.4 GiB/s through the cell's own reads (`read_stored`, an env-var switch on the same build),
3.1 GiB/s on the old engine (`storage/io.rs`, io_uring on the segment files). Mostly off-CPU: each
1 MiB part paged into a memfd slab (page allocation, zeroing, cgroup charge), read back out with
`pread`, then evicted (`fallocate` punching holes), three parts ahead at most. With the device
reading straight into Fxfs's buffer, through the cell (`OpenCell::read_uncached`), 2.8 GiB/s.
**Why it's dead:** the filesystem mounted on the device caches what it reads in its kernel's pages
(`crate::pages`); the extent cache holds the same bytes a second time, and the cell's page cache a
third, all under one budget. The extent cache is for what windows map and their guests read, which
no host mount does.
**Reopen if:** a host mount and VMs are seen reading the same sealed extents at once, or host
mounts map extents' pages rather than copy them.
