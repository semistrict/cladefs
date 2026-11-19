---
id: TASK-658.3
title: 'Explainer film: multi-region, cells across buckets'
status: Done
assignee: []
created_date: '2026-11-19 15:28'
updated_date: '2026-11-19 16:13'
labels:
  - explainer
dependencies: []
references:
  - TASK-633
documentation:
  - docs/operating-a-host.md
parent_task_id: TASK-658
type: docs
ordinal: 1041000
---

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A 45-75 s film shows a bucket per region, hosts finding buckets by number, and forks across buckets, marking what TASK-633 has not landed
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
explainer/regions/, 72 s: a sandbox in europe-west1 whose cell carries bucket 2's number; format-bucket/add-bucket, the registry, routing by the cell ID, part caches per cloud region; forks across buckets and mirrors marked in progress (TASK-633.7, .8). Verified: check 0 errors, independent fact check (5 wording fixes), render 72 s at -16.1 LUFS.
<!-- SECTION:FINAL_SUMMARY:END -->
