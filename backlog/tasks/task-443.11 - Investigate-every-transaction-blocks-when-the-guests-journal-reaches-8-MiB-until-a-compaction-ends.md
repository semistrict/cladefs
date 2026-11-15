---
id: TASK-443.11
title: >-
  Investigate: every transaction blocks when the guest's journal reaches 8 MiB
  until a compaction ends
status: To Do
assignee: []
created_date: '2026-11-15 12:25'
labels:
  - perf
  - review-2026-11-15
dependencies: []
documentation:
  - docs/reviews/2026-11-15-sandbox-performance.md
parent_task_id: TASK-443
priority: medium
ordinal: 845000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/fxfs/src/object_store/journal.rs check_journal_space waits on reclaim_event once the journal passes reclaim_size, 8 MiB at most in the guest (guest/rust/fxfs-linux/src/fs.rs JOURNAL_MAX); compaction starts at half; a size-tiered merge rewrites a layer as large as the newer ones together, and the non-Fuchsia CompactionYielder::yield_now is empty. Stalls grow with the filesystem's size and age. Found by reading the code, not measured (docs/reviews/2026-11-15-sandbox-performance.md).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 journal_space_waits, journal_space_wait_ns, journal_compactions and journal_compaction_ns are read around the untar and create-parallel cases on an image holding ~1M files, with a disk loader in the same bench job
- [ ] #2 The notes give the measured cost and the workload size at which a customer would feel it; a fix, if needed, is filed as its own task
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
