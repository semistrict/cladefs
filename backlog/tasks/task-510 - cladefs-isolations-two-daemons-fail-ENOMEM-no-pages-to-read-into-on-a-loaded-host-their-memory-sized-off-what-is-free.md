---
id: TASK-510
title: >-
  cladefs::isolation's two daemons fail ENOMEM ('no pages to read into') on a
  loaded host: their memory sized off what is free
status: To Do
assignee: []
created_date: '2026-11-18 01:21'
labels:
  - tests
  - flaky
dependencies: []
priority: low
ordinal: 897000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
ubu1, 2026-11-17, branch t456-4 (99ef3542), fxjob --mem 8G beside another agent's 12 GiB job, 274 cladefs/extent_files/sqlite_extents tests at --test-threads 4: vm_processes_reach_their_daemons_memory_alone aborted (SIGABRT, leaked handles) after 'reading ExtentId {..} at 0x0: Errno(ENOMEM): no pages to read into' and 'unresolved DAX window fault at offset 0x0 (Read): ENOMEM'. Run alone it passed. The test's daemons start with no memory argument, so each sizes its page cache off what the host and cgroup have free (TASK-358's rule), which other tests and jobs in the same cgroup leave little of; as TASK-442 saw for guest cases. A test daemon should be given a fixed memory (as test-e2e.sh's CLADEFSD_MEMORY).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The isolation test's daemons run with a fixed memory, independent of what the host has free
- [ ] #2 The test passes in the full cladefs run under fxjob beside another job
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
