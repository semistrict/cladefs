---
id: TASK-539.2
title: 'Dir::give replaces Dir::host_path'
status: Done
assignee:
  - '@claude'
created_date: '2026-11-18 14:16'
updated_date: '2026-11-18 16:49'
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
- [x] #1 Dir::give(&self, path, to: &mut dyn FnMut(&Path) -> io::Result<bool>) -> io::Result<bool> exists. DiskDir calls `to` with the joined path, MemDisk returns Ok(false), and CachedDir settles and cancels the file's pages (moved from its host_path) and then forwards.
- [x] #2 leave() in crates/local_cell/src/cell.rs calls dir.give(&path, &mut |from| cache.adopt(&key, from)) and removes the file only when it was not given.
- [x] #3 Recovering::open's device check uses Arc::ptr_eq(dir.disk(), cache.disk()). rg finds no host_path in crates/.
- [x] #4 New cached.rs test: a dirty data file that is given is written back before `to` runs and its pages are gone; one whose `to` returns false keeps its pages and stays. adopts_whole_objects and the TASK-359 refusal test pass unchanged.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Dir::give replaces host_path; leave() gives files to the part cache; the cache-filesystem check compares disks' devices. Tests: cached::tests::files_given_are_written_back_and_taken_files_leave, a_directory_in_memory_gives_nothing; stores_elsewhere_than_the_hosts_cache_are_refused on Linux. Linux (ubu1, fxjob, 9d5012c..2d67074): clippy --workspace --tests clean; local_cell+bucket_cell 590/593, cladefs 666/669, rest of workspace 1680/1686, every failure one of a1167a56a's regression tests for open bugs (TASK-516..536), failing on the base too. macOS: the same crates' suites, same three pre-existing audit failures.
<!-- SECTION:FINAL_SUMMARY:END -->
