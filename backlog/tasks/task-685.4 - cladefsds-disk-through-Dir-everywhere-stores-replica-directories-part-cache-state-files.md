---
id: TASK-685.4
title: >-
  cladefsd's disk through Dir everywhere: stores, replica directories, part
  cache, state files
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-20 03:33'
updated_date: '2026-11-20 21:00'
labels:
  - testing
  - simulation
dependencies: []
parent_task_id: TASK-685
ordinal: 1070000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-24 step 4: every file cladefsd writes goes through local_cell::Dir, so a simulated node's disk is a MemDir crashed with crash_corrupting on its kill.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 No std::fs in cladefsd's serving paths outside Dir implementations; a lint refuses new ones
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. host_fs crate: std::fs re-exported built normally (zero cost), with the descriptor calls beside it (punch_hole, data_ranges, sync_with, Flock) and FileExt/MetadataExt/OpenOptionsExt; under madsim a disk in memory per simulated host (thread, node), durable as files and directories are synced, crashed at the host's first call after a restart: unsynced entries and writes kept, lost, garbage or torn as the seed says (MemDisk's model, path-based).
2. Flock moves from node_local to host_fs: a simulated host's locks are its disk's, gone with its crash.
3. local_cell, bucket_cell and cladefs's serving files (host_cell, open_cell, cell_dir, profile, ephemeral_host, cladefsd cells/server/peers/boot) through host_fs; Path::exists/is_dir/canonicalize through host_fs; DiskDir, DiskFile, the part cache's Files and HostDisk::of on host_fs (statvfs a terabyte under madsim, no page cache to bypass).
4. Lint off-host-fs (scripts/rust-rules.py): std::fs, its traits, Path::exists/canonicalize in those crates outside host_fs, unless said outside the simulation or in a file that is (FUSE, VM sockets, client, procfs, import source, file bucket, tools, tests).
5. Prove: host_fs madsim tests (durable kept through a kill, what a crash keeps the seed's, locks gone); the attrition profiles over 100+ seeds with each host's disk crashed at its restart; workspace on macOS and Linux.
<!-- SECTION:PLAN:END -->
