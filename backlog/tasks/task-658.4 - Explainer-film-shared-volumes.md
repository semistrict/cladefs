---
id: TASK-658.4
title: 'Explainer film: shared volumes'
status: Done
assignee: []
created_date: '2026-11-19 15:29'
updated_date: '2026-11-19 16:13'
labels:
  - explainer
dependencies: []
documentation:
  - backlog/docs/doc-10 - Shared-volumes-Fxfs-volumes-over-read-only-bases.md
parent_task_id: TASK-658
type: docs
ordinal: 1042000
---

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A 45-75 s film shows one sandbox keeping a Git checkout current and others attaching it under writable layers of their own
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
explainer/shared-volumes/, 72 s: doc-10's Linux checkout published as base 1, attached by running sandboxes under layers of their own; sandbox manager marked not built, git fetch onto base 2 marked in progress (TASK-136.7). Verified: check 0 errors, independent fact check (7 fixes), render 72 s at -16.1 LUFS.
<!-- SECTION:FINAL_SUMMARY:END -->
