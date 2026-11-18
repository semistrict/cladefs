---
id: TASK-539.15
title: >-
  Commit records grow a store's disk unadmitted: about 115 bytes a commit, past
  a tenant's limit while it is full
status: To Do
assignee: []
created_date: '2026-11-18 15:35'
labels:
  - extentd
  - disk
dependencies: []
documentation:
  - backlog/docs/doc-22 - Disk-governor-the-hosts-disk-as-a-managed-resource.md
parent_task_id: TASK-539
priority: medium
type: bug
ordinal: 947000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found writing TASK-539.7's full-disk test (crates/local_cell/tests/reserve.rs): each commit appends about 115 bytes of records to its open segment's log (1,000 empty commits: the log at 123 KiB, 127 KiB once framed), which no admission covers, as commits are never refused (doc-7 decision 3, TASK-94: commits go on, as their frees and uploads make room). A guest that commits again and again while its writes are refused takes that much each time: on a host, the disk governor's headroom absorbs it (4 GiB is some 37 million commits); against a tenant's limit, which doc-22 makes a hard bound, its commits' records pass it. The reserve's measurement (RESERVE_PER_STORE, 64 KiB a store open) covers a bounded number of them, not a storm. Options to weigh: commits that change nothing but the root writing no record, or a smaller one; records counted against the limit as pledges that refuse the next write sooner; or the open segment closed, framed and its log let go once its log passes a bound.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A test commits, without writing, many times on a store whose tenant's limit and disk are full, and the store's footprint stays within the limit plus a stated bound, or the design states why commits' records may pass it and by how much
- [ ] #2 doc-22 states what commits' records take and what bounds them
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
