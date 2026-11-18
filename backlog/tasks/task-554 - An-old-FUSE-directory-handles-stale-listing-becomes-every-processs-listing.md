---
id: TASK-554
title: An old FUSE directory handle's stale listing becomes every process's listing
status: To Do
assignee: []
created_date: '2026-11-18 15:25'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 945000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), fuse-sim review; verified against the code by independent reviewers. Severity: high.

**Where:** `crates/cladefs/src/fuse/server.rs`: `rg -n -F 'let listed = dots.iter().chain(&open.entries).enumerate().skip(offset as usize);' crates/cladefs/src/fuse/server.rs`

OPENDIR copies the directory's entries into the handle's OpenDir (open_dir, line 1163), and every READDIR/READDIRPLUS on that handle lists from that copy, at offset 0 too. libfuse refreshes at offset 0 ('According to SUS, directory contents need to be refreshed on rewinddir()'); this server never does. The server also replies to OPENDIR with FOPEN_KEEP_CACHE | FOPEN_CACHE_DIR (line 1127). With those flags, Linux fs/fuse/readdir.c fills the directory inode's own readdir cache from whichever handle first reads it from position 0 after a reset or on an empty cache. It stamps that cache with the inode's current i_version and i_mtime (fuse_readdir_cached sets rdc.iversion/rdc.mtime and returns UNCACHED). fuse_emit then adds each entry of that handle's reply (fuse_add_dirent_to_cache), and fuse_readdir_cache_end marks the cache complete at EOF. So the listing of a handle opened before a change gets cached as the listing of the changed directory. Every later opendir sees it, because KEEP_CACHE keeps the cache and the version and mtime checks pass, until the directory changes again. Separately, a process that rewinds its own handle never sees entries created after it opened the directory, which SUS requires it to see.

**Trigger.** Linux FUSE mount (cladefs-mount, or cladefsd's CLADEFSD_MOUNT of cells), directory m/d holding {a}. 1) Process A: fd = open("m/d", O_DIRECTORY): OPENDIR, and open_dir copies {a} into OpenDir fh1. 2) Process B: creat("m/d/b"): CREATE; the kernel's fuse_dir_changed bumps d's i_version and invalidates its attributes. 3) Any path walk through d, e.g. stat("m/d/b"): fuse_permission (default_permissions) sends GETATTR for d, so i_mtime is now current. 4) A: getdents64(fd) at position 0: the cache is empty or reset, rdc.iversion/rdc.mtime get the current values, and READDIRPLUS(fh1, 0) is sent. Server::readdir lists the step-1 copy {., .., a}, which the kernel caches as complete. 5) Any process: ls m/d: a new OPENDIR with KEEP_CACHE, then readdir at position 0 is served from d's cache with version and mtime matching, so b is missing although stat m/d/b succeeds. This lasts until d changes again. cp -r, tar, rsync, du and backups over the mount silently skip b. A process holding a directory fd and rewinding it to poll refills the cache from its old copy after every change, so listings stay stale as long as it runs.

**Why the tests stay green.** fuzz_tests.rs checks reply shapes, rising offsets and the server's tables, never whether a listing matches the directory as it is now. fuse_differential.rs compares std::fs::read_dir after each call, which opens the directory fresh every time and never holds a directory fd across a change. fuse.rs's readdirplus_skips_removed_entries reads offset 0 of a handle opened before a change, but only checks that removed names are left out, which a refresh at offset 0 would also pass. No test pairs a handle opened before a create with the kernel's directory cache.

**Backlog.** Related: TASK-25.9 (Done) added FOPEN_CACHE_DIR, and TASK-17 (Done) made READDIRPLUS skip entries removed since OPENDIR. Neither deals with a handle's copy served at offset 0 or with it becoming the kernel's readdir cache.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 On the FUSE mount (cladefs-mount, or the daemon's CLADEFSD_MOUNT), a READDIR at offset 0 on a handle opened before a CREATE in that directory lists the created file. Rewinding a directory handle lists the directory as it is now.
- [ ] #2 Suppose a process reads, from position 0, a directory fd it opened before a create, after a stat in that directory. A fresh `ls` of the directory in any process afterwards still lists the new file: the kernel's readdir cache never holds an older handle's listing.
- [ ] #3 Regression tests exist at two levels and go red with the fix reverted. The server-level one in fuse.rs does OPENDIR, CREATE dir/b, then READDIR(fh, 0) and expects b. The Linux-mount one in fuse_mount.rs opens a nix::dir::Dir, creates m/d/b, stats m/d, iterates the old Dir from the start, then asserts read_dir("m/d") lists b and a rewound old Dir lists b.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
