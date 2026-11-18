---
id: TASK-543
title: Forks fail with EIO when the temp dir is not on the part cache's filesystem
status: To Do
assignee: []
created_date: '2026-11-18 15:23'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 934000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), cross-host-take-restore review; verified against the code by independent reviewers. Severity: high.

**Where:** `crates/cladefs/src/host_cell.rs`: `rg -n -F 'std::env::temp_dir().join(format!("cladefs-versions-{}", std::process::id()))' crates/cladefs/src/host_cell.rs`

Two kinds of store are made in std::env::temp_dir(): the daemon's kept version stores for restores (cells.rs opened_version -> version_dir) and fork_through's temporary version stores, which every fork of a live store now goes through (OpenCell::fork_at_version, since 850e21e0a). Both are opened with the daemon's uploader options, which include the part cache: cells.rs keeping() sets `cache: part_cache()?`, and fork_through uses `uploader.options().clone()`. Shared::start's version branch passes that cache to Recovering::open, which unconditionally requires the store's directory and the cache to be on one device (crates/local_cell/src/cell.rs:907, `device(&here)? == device(cache.dir())?,`, TASK-359). The check only makes sense for stores whose acknowledged segments are renamed into the cache, and a version store has no write-ahead log. With CLADEFSD_PART_CACHE on a data disk beside the stores (which TASK-359 requires of the stores) and TMPDIR unset, the temp dir is /tmp on the root filesystem or a tmpfs. Then every version store fails to open with 'the store at /tmp/cladefs-versions-<pid>/... is on another filesystem than the host's cache', which errno_of turns into EIO. On kernels whose tmpfs refuses O_DIRECT, direct::bypass also fails these files even without a cache. Nothing checks this at daemon start, which AGENTS.md asks for ('Check such a precondition up front ... two directories on one filesystem'), and no doc says TMPDIR must share the cache's filesystem. Secondary: the root is a predictable name in a world-writable /tmp, created with create_dir_all by a root daemon.

**Trigger.** Start cladefsd with CLADEFSD_PART_CACHE=/var/lib/cladefs/cache (same disk as the stores) and the default TMPDIR, where /tmp is tmpfs (Debian 13 / Fedora default) or the root disk. Restore any VM snapshot (OPEN with ForkOf, a memory_cell restore, or ForkCell with a version): fork_version -> opened_version makes /tmp/cladefs-versions-<pid>/t.c.v -> kept_or_open -> take_store -> OpenCell::take_with -> host_cell::take -> upload::open -> Shared::start (version branch) -> Recovering::open -> the ensure! fails, and the restore fails with EIO. Likewise a VM's Request::Fork or ForkCell from a live store: fork_at_version -> fork_through -> open(&at, options with cache) fails the same way.

**Why the tests stay green.** scripts/test-e2e.sh puts the daemon's cache under $results, inside the job's TMPDIR, which is also the daemon's temp dir. Unit and daemon tests make caches in tempfile dirs under temp_dir(). So the version dir and the cache always share a filesystem in tests. The TASK-359 refusal test only covers a regular store placed elsewhere than the cache.

**Backlog.** New. Nearest is TASK-359 (In Progress), whose store/part cache device check this trips but which has no case for version stores; TASK-514 (To Do) touches fork_through only for a host dying mid-fork.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 With CLADEFSD_PART_CACHE on a different filesystem from the daemon's temp dir (/tmp on tmpfs or the root disk), forking a live store (a VM's Request::Fork, ForkCell) and forking a version (a VM snapshot restore through OPEN with ForkOf, a memory_cell restore, ForkCell with a version) all succeed instead of failing with EIO.
- [ ] #2 No layout cladefsd starts on makes a fork fail on a filesystem mismatch: either forks work, or the daemon refuses to start with an error naming both directories.
- [ ] #3 A cladefsd test puts the part cache on another device than temp_dir() (or uses a failpoint that reports another device for version stores), makes a version, then calls fork_of_version and fork and asserts both succeed; it goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
