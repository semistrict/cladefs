---
id: TASK-443.9
title: >-
  Investigate: stat of a just-written file writes it back in the foreground, and
  every regular-file stat takes the journal's writer mutex
status: To Do
assignee: []
created_date: '2026-11-15 12:25'
updated_date: '2026-11-17 01:10'
labels:
  - perf
  - review-2026-11-15
dependencies: []
documentation:
  - docs/reviews/2026-11-15-sandbox-performance.md
parent_task_id: TASK-443
priority: medium
ordinal: 843000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
guest/linux/fs/cladefs/super.c fxfs_vfs_getattr calls filemap_write_and_wait when inode_get_bytes(inode) < i_size_read(inode) with dirty pages (TASK-138, xfstests generic/422 and 615); fxfs_vfs_refresh_blocks reads Journal::file_offset, which locks writer_mutex, held across every commit's apply. git checkout and make stat what they write: one device round trip a file, and stats queue behind commits. Found by reading the code, not measured (docs/reviews/2026-11-15-sandbox-performance.md).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A fs-bench case creating a file, writing 1 KiB, fstat, close against the same without the fstat: files a second and device_writes for both; and stat-parallel beside a concurrent creator
- [ ] #2 The notes give the measured cost and the workload size at which a customer would feel it; a fix, if needed, is filed as its own task
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
