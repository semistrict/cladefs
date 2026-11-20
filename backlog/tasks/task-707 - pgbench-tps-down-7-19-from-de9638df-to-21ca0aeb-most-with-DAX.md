---
id: TASK-707
title: 'pgbench tps down 7-19% from de9638df to 21ca0aeb, most with DAX'
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-20 21:24'
updated_date: '2026-11-20 21:24'
labels:
  - perf
  - regression
dependencies: []
priority: high
ordinal: 1111000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Reported by the sandbox team (GCE 16 vCPU, Firecracker nested, 4-vCPU VM, pgbench scale 50, 4 clients, 3x60 s, shared_buffers=256MB, fresh cell). Mean tps: DAX default window -16%, 8 GiB window -9%, 4 GiB window -19%, no DAX -7%; local-block baseline -2%, overlay -5%. bun install workload and memory unchanged. Suspects in range: fd7f4b84a (commit path async: every request a future via serve_future, deferred commits woken on the Pool), 8b5722b41/f33db2873 (TASK-690: head swapped at most once a second on GCS, acknowledgement beside uploads; may gate within_lag/uploaded waits), 9a04595e7 (TASK-672: every span recorded for /z/tracez, ~497 ns each, global state).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 An interleaved A/B on ubu1 (fxfs-guest-pgbench, -dax) reproduces or rules out the drop
- [ ] #2 The commit responsible is named, by bisecting de9638df..21ca0aeb
- [ ] #3 The drop is fixed, or its cost is accepted and documented with numbers
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
