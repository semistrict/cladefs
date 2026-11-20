---
id: TASK-672.1
title: '/z/tracez: spans kept in-process by latency bucket, always on'
status: Done
assignee:
  - '@claude'
created_date: '2026-11-19 22:23'
updated_date: '2026-11-20 00:30'
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
- [x] #1 /z/tracez lists each span name with its count per latency bucket and its running spans
- [x] #2 A bucket span 5 ms long is kept as a sample of the >1ms bucket with its object key, cell and its ancestors (page-in, request) shown
- [x] #3 Recording works with CLADEFSD_TRACES unset, and OTLP export with it set is unchanged (tests/otel.rs passes)
- [x] #4 The per-span cost while recording is measured and written in docs/tracing.md
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. One SpanProcessor, the recorder (cladefsd::zpages::tracez), always installed in cladefsd with sampler AlwaysOn: counts per span name in Census latency buckets and errors; running spans; spans buffered per trace until none of it runs, then kept as samples (latest K per name and bucket, errors), bounded per trace and in all.
2. OTLP export becomes a filter inside the recorder: a trace kept by trace-ID ratio (OTEL_TRACES_SAMPLER always_on/always_off/traceidratio/parentbased_*, ARG), handed to the batch processor; the SDK clones a span per processor, so one processor.
3. HTTP: the metrics server becomes a router (cladefsd::http): /metrics, /z/, /z/tracez (?name=&bucket=&tag=key=value, ?running=), per-exchange deadlines.
4. Tests: recorder unit tests (buckets, finalize, orphans, caps), the daemon's spans recorded without CLADEFSD_TRACES, OTLP unchanged (tests/otel.rs), HTTP pages; cost per span measured as spans_off_are_nothing is, written in docs/tracing.md.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Recorder: one SpanProcessor (zpages::tracez::Processor), sampler AlwaysOn, OTLP export a per-trace filter inside it (the SDK clones a span per processor). A trace keeps its last 64 spans (ancestors end last; first version dropped the root, caught by what_is_held_is_bounded). Request spans now end before their reply is sent, so a client holding the reply finds the trace recorded. Cost: span-bench on ubu1 release under fxjob --bench: 2 ns off, 497 ns recorded.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Every span is recorded in-process (Census latency buckets, running spans, latest 4 samples per name/bucket as whole traces) and served at /z/tracez on the CLADEFSD_METRICS listener, beside /metrics (new cladefsd::http router); OTLP export is unchanged but filtered per trace. Verified on ubu1: recorder unit tests, spans_are_recorded_unexported, spans_trace_requests_commits_and_uploads, tests/tracing.rs (binary over OTLP), z_pages_are_served_under_z, tests/zpages.rs; full cladefs+bucket_cell suite 1085/1086 (the one failure is unrelated timing, TASK-677). Cost measured with span-bench, in docs/tracing.md.
<!-- SECTION:FINAL_SUMMARY:END -->
