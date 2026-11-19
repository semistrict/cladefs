---
id: TASK-658.5
title: 'Explainer film: container images as bases'
status: Done
assignee: []
created_date: '2026-11-19 15:29'
updated_date: '2026-11-19 16:13'
labels:
  - explainer
dependencies: []
documentation:
  - README.md
parent_task_id: TASK-658
type: docs
ordinal: 1043000
---

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A 45-75 s film shows cladefsctl import turning an image into a base, forks of it booting, and sharing what they read
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
explainer/images/, 62 s: README's Try it (exports, docker export alpine, cladefsctl import, fork, stat with measured numbers) to a hundred forks sharing one copy within a tenant. Found README's Try it missing CLADEFS_MASTER_KEY, fixed in c77f187dc. Verified: check 0 errors, independent fact check (5 fixes), render 62 s at -16.2 LUFS.
<!-- SECTION:FINAL_SUMMARY:END -->
