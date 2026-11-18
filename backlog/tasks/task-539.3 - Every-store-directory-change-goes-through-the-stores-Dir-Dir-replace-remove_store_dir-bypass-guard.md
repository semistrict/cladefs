---
id: TASK-539.3
title: >-
  Every store-directory change goes through the store's Dir (Dir::replace,
  remove_store_dir, bypass guard)
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18 14:16'
updated_date: '2026-11-18 14:50'
labels:
  - extentd
  - disk
dependencies: []
documentation:
  - backlog/docs/doc-22 - Disk-governor-the-hosts-disk-as-a-managed-resource.md
parent_task_id: TASK-539
priority: medium
type: feature
ordinal: 935000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Exact counting needs no path around the layer. Today place_host, made_host, cell_dir::write/mark, profiles and five remove_dir_all sites use std::fs directly. (doc-22 task T3.)
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Dir::replace(path, bytes) is a required method. DiskDir writes .new, syncs, renames over and syncs the directory. MemDisk adds a DirChange::Replace that the crash model cuts, and a crash test shows a replace leaves the old file or the new one, never neither. Dir::rename keeps refusing an existing `to` on MemDisk.
- [ ] #2 place_host, made_host, cell_dir::write, cell_dir::mark and profile writing go through HostDir.
- [ ] #3 local_cell::remove_store_dir(path) replaces host::remove_store, cells.rs removed_if_unmade's remove_dir_all, and host_cell.rs's three `let _ = std::fs::remove_dir_all(...)`.
- [ ] #4 A test greps non-test sources of local_cell, cladefs and bucket_cell for std::fs::(write|File::create|rename|remove_file|remove_dir_all|hard_link) and OpenOptions. It fails on any site outside the allowlist (dir.rs, cache.rs, direct.rs, host_disk.rs, remove_store_dir, the daemon's sockets and stats file).
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Records beside a store (description placing, made marker, cladefs's attached-bases/ancestry/public markers/profiles) go through HostDir: Dir::replace (DiskDir: .new written direct, synced, renamed, dir synced; MemDisk: DirChange::Replace that a crash keeps or not; CachedDir forgets a replaced data file's pages). Every removal of a store's directory goes through local_cell::remove_store_dir (host_cell::discard logs failures of a failed making's cleanup). Deviation: AC #4's grep guard is replaced by a behavioral one in TASK-539.4/539.5 (a store's footprint equals a fresh listing after cladefs's record writes), since a file allowlist can't tell a store directory's writes from the many legitimate std::fs writes elsewhere (bucket file://, cache, stats, sockets).
<!-- SECTION:NOTES:END -->
