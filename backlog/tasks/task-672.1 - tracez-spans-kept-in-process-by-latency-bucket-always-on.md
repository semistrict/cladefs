---
id: TASK-672.1
title: '/tracez: spans kept in-process by latency bucket, always on'
status: To Do
assignee: []
created_date: '2026-11-19 22:23'
labels: []
dependencies: []
parent_task_id: TASK-672
ordinal: 1045000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Every span (request, page-in, bucket request, commit, upload) is recorded in-process whatever OTLP does, so its latency counts in Census buckets (>0, >10us, >100us, >1ms, >10ms, >100ms, >1s, >10s, >100s) per span name, and a few samples of each bucket are kept whole: attributes, parent chain. Wall clock is the point: bucket requests take milliseconds, so they fill the slow buckets themselves. The cost moves from one atomic load per span (tracing off) to a real span per request; it must be measured, as the request path makes thousands a second.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 /tracez lists each span name with its count per latency bucket and its running spans
- [ ] #2 A bucket span 5 ms long is kept as a sample of the >1ms bucket with its object key, cell and its ancestors (page-in, request) shown
- [ ] #3 Recording works with CLADEFSD_TRACES unset, and OTLP export with it set is unchanged (tests/otel.rs passes)
- [ ] #4 The per-span cost while recording is measured and written in docs/tracing.md
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
