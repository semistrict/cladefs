---
id: TASK-407.3
title: >-
  Code: the collector keeps what heads name and what their epochs may yet name;
  pins gone
status: Done
assignee: []
created_date: '2026-11-15 01:25'
updated_date: '2026-11-15 06:33'
labels: []
dependencies: []
parent_task_id: TASK-407
priority: high
ordinal: 800000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
collector::pinned, orphans_after_successor and extent_keys_name_segments replaced by specs/bucket's collectorEpochs: a live branch's objects its head does not name are deleted unless of the head's epoch (or later) and at or past its end (segments, extents of their own) or newer than its checkpoint (checkpoints). Heads are never deleted. Recoveries from heads.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The collector's fates follow collectorEpochs; NoCollectorEpochs' scenario is a test that fails without it
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Landed with TASK-407.2: collector pins, orphans_after_successor and extent_keys_name_segments replaced by collector_epochs (Ended::ahead); heads never deleted; recoveries from heads. NoCollectorEpochs' scenario is segments_uploaded_not_yet_acknowledged_are_kept / _need_collector_epochs, plus sim::collection_needs_epochs. Same verification as TASK-407.2 (macOS, ubu1).
<!-- SECTION:FINAL_SUMMARY:END -->
