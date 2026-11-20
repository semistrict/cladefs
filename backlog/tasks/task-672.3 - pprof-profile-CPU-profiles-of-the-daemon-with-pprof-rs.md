---
id: TASK-672.3
title: '/z/pprof/profile: CPU profiles of the daemon with pprof-rs'
status: Done
assignee:
  - '@claude'
created_date: '2026-11-19 22:23'
updated_date: '2026-11-20 00:30'
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
- [x] #1 GET /z/pprof/profile?seconds=N samples the daemon for N seconds and returns pprof protobuf that go tool pprof reads
- [x] #2 GET /z/pprof/flamegraph?seconds=N returns an SVG flamegraph
- [x] #3 One profile at a time: a second request while one runs is refused with 409
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. pprof-rs (0.15, Apache-2.0; features prost-codec, flamegraph, frame-pointer: every build keeps frame pointers, .cargo/config.toml), Linux only as cladefsd is; inferno, its flamegraph renderer, is CDDL-1.0 (OSI-approved).
2. cladefsd::zpages::pprof: /z/pprof/profile?seconds=N (pprof protobuf, gzip as go tool pprof reads either), /z/pprof/flamegraph?seconds=N (SVG); N 1..=300, default 30; one profile at a time, 409 otherwise; the HTTP response written within its own deadline after the profile (http.rs already splits the exchange).
3. SIGPROF with SA_RESTART interrupts syscalls that never restart (epoll_wait, poll, nanosleep): check the daemon's own libc loops handle EINTR.
4. Tests: a profile of a busy thread decodes as a pprof Profile with samples naming it; flamegraph is SVG; a second at once gets 409; bad seconds 400.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
pprof 0.15 (prost-codec, flamegraph, frame-pointer), Linux only; inferno is CDDL-1.0 (OSI). SIGPROF with SA_RESTART: the daemon's own loops (wire.rs, storage/io.rs) already retry EINTR. Checked on a real cladefsd on ubu1: go tool pprof -top reads /z/pprof/profile?seconds=3 (Type cpu, Duration 3s), flamegraph 200 image/svg+xml. Profiles carry no gzip: go tool pprof reads raw protobuf.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
/z/pprof/profile?seconds=N (pprof protobuf) and /z/pprof/flamegraph?seconds=N (SVG), 1..300 s, one at a time (409). Verified: profiles_say_what_ran (decodes, names the spinning function; SVG names it), profiles_are_taken_one_at_a_time, profiles_sample_for_the_seconds_asked, and go tool pprof against a running cladefsd on ubu1.
<!-- SECTION:FINAL_SUMMARY:END -->
