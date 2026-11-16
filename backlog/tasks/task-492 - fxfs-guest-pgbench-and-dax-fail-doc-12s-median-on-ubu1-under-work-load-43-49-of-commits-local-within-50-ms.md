---
id: TASK-492
title: >-
  fxfs-guest-pgbench and -dax fail doc-12's median on ubu1 under work load:
  43-49% of commits local within 50 ms
status: To Do
assignee: []
created_date: '2026-11-16 10:08'
labels:
  - bug
  - perf
  - durability
dependencies: []
ordinal: 882000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen 2026-11-16 in plain test-host.sh runs (fxjob work slice, other agents' jobs beside, ubu1's scratch disk, which others filled to ENOSPC once that day). Both cases' check_durable_median fails, p50 64 ms, on main and on the renames branch alike: main d4a68e52e (run t474m): 308,149/673,870 and 282,960/661,212 within 50 ms; branch 1114a9f72 (t474d): 294,027/603,013 and 271,114/593,467. The same case passed at 81.3% on main 10f613290 on 2026-11-13 under fxjob --bench (TASK-322 notes). So either the check is not met beside other jobs on ubu1's shared disk (TASK-322's cause) or main regressed since 2026-11-13; plain runs fail CI-style on ubu1 either way.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Main measured in fxjob --bench (alone) on ubu1 says whether the case still meets doc-12's median; if not, the regression is found by bisecting from 10f613290
- [ ] #2 If it holds alone: the case says what it needs (a bench job), or plain test-host.sh runs leave it out, so that a loaded host does not fail every run
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
