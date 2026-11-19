---
id: TASK-672
title: >-
  z-pages for cladefsd under /z/: in-process tracez and CPU profiles on the
  metrics listener
status: To Do
assignee: []
created_date: '2026-11-19 22:22'
updated_date: '2026-11-19 23:31'
labels: []
dependencies: []
ordinal: 1044000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The user (ex-Google) wants Census-style z-pages: to look at a running daemon and see which reads missed every cache (extent cache, part cache, peers) and went to the bucket, with the stack that caused them and the tags (cell, extent, object key, request) on them. Today spans only leave over OTLP, sampled 1 in 100, and only while CLADEFSD_TRACES is on, so a slow bucket read in production is invisible. Rust has no maintained equivalent: opentelemetry-zpages is deprecated (0.14.0+deprecated), so cladefs builds its own. Decisions (2026-11-19): spans recorded in-process always, keeping only slow or cache-missing ones whole; CPU profiles via pprof-rs untagged; served on the CLADEFSD_METRICS listener next to /metrics.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 GET /z/tracez on the CLADEFSD_METRICS listener shows, per span name, counts in Census latency buckets and kept sample spans with their attributes, without CLADEFSD_TRACES
- [ ] #2 GET /z/pprof/profile?seconds=N returns a CPU profile
- [ ] #3 GET /z/ lists every z-page
- [ ] #4 docs/tracing.md documents the pages
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
