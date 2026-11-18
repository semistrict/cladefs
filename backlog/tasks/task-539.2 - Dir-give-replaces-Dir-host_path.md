---
id: TASK-539.2
title: 'Dir::give replaces Dir::host_path'
status: To Do
assignee: []
created_date: '2026-11-18 14:16'
labels:
  - extentd
  - disk
dependencies:
  - TASK-539.1
documentation:
  - backlog/docs/doc-22 - Disk-governor-the-hosts-disk-as-a-managed-resource.md
parent_task_id: TASK-539
priority: medium
type: feature
ordinal: 934000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
leave() renames a store's file into the part cache outside Dir (`dir.host_path(&path)` then `cache.adopt(&key, &from)`), so no layer can see whether the file left. (doc-22 task T2.)
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Dir::give(&self, path, to: &mut dyn FnMut(&Path) -> io::Result<bool>) -> io::Result<bool> exists. DiskDir calls `to` with the joined path, MemDisk returns Ok(false), and CachedDir settles and cancels the file's pages (moved from its host_path) and then forwards.
- [ ] #2 leave() in crates/local_cell/src/cell.rs calls dir.give(&path, &mut |from| cache.adopt(&key, from)) and removes the file only when it was not given.
- [ ] #3 Recovering::open's device check uses Arc::ptr_eq(dir.disk(), cache.disk()). rg finds no host_path in crates/.
- [ ] #4 New cached.rs test: a dirty data file that is given is written back before `to` runs and its pages are gone; one whose `to` returns false keeps its pages and stays. adopts_whole_objects and the TASK-359 refusal test pass unchanged.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
