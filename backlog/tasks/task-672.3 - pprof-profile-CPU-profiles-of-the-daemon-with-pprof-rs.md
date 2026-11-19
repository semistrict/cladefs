---
id: TASK-672.3
title: '/z/pprof/profile: CPU profiles of the daemon with pprof-rs'
status: To Do
assignee: []
created_date: '2026-11-19 22:23'
updated_date: '2026-11-19 23:31'
labels: []
dependencies: []
parent_task_id: TASK-672
ordinal: 1047000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
A CPU profile of a running daemon, as Go serves at /debug/pprof/profile, taken on demand over the metrics listener. pprof-rs (tikv, Apache-2.0) samples with SIGPROF; it cannot tag samples with cells or requests, which the user accepted (2026-11-19).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 GET /z/pprof/profile?seconds=N samples the daemon for N seconds and returns pprof protobuf that go tool pprof reads
- [ ] #2 GET /z/pprof/flamegraph?seconds=N returns an SVG flamegraph
- [ ] #3 One profile at a time: a second request while one runs is refused with 409
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
