---
id: TASK-707
title: 'pgbench tps down 7-19% from de9638df to 21ca0aeb, most with DAX'
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-20 21:24'
updated_date: '2026-11-21 07:41'
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

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Read the range's hot-path changes (no bisect, user 2026-11-20). 2. Hypothesis: 9a04595e7 (TASK-672.1) makes every span real: otel::install always builds a provider with Sampler::AlwaysOn and the tracez Processor, so trace::enabled() is true and each request, page-in, commit phase and bucket request pays SDK span creation, attributes, random IDs, two shard locks and a Named lookup; before, spans were off (2 ns) unless CLADEFSD_TRACES=otlp, then 1% sampled. Fits the pattern: DAX makes the most requests (first-store CoW faults), a smaller window more; no-DAX fewest. 3. Test: 21ca0aeb with an experiment-only env switch skipping install, same binary on/off, fxfs-guest-pgbench-dax on ubu1 under fxjob --bench.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-20 ubu1, main f247775cf, fxfs-guest-pgbench-dax, 3x30 s, same binary, interleaved, fxjob --bench: spans recorded (unset) 2853/2918/2832 and 2690/2960/2864 tps, mean 2853; CLADEFSD_TRACES=off (daemon logged 'spans off') 2808/2938/2924 and 2287/2922/2828, mean 2785 (2884 without the 2287 first run). No difference beyond noise: span recording does not explain the drop on ubu1 (not nested virt; their GCE is). CLADEFSD_TRACES=off landed anyway (f247775cf) for the sandbox team's retest. Every run failed its encryption check after pgbench: TASK-711.

2026-11-21: ubu1 reproduces it on its file bucket (no GCS spacing): interleaved A/B, fxfs-guest-pgbench-dax 3x30 s, de9638df 2987/2972/3049 (round 2 lost to systemd 'Transport endpoint is not connected'), main 7b6195f54 2229/2705/2833 and 2667/2808/2620: about 12% down. Not GCS's head cadence then (21ca0aeb's 1/s head swap, before TASK-691's slots, may add to it on GCS). Bisecting first-parent at 882e2daf8 (after zpages + replicated commits), e21e4efc4 (after per-node statics), 305aa2033 (after TASK-690), 21ca0aeb7 (after the async merge 5f65a04ee): ubu1 ~/src/fxdax-pgb-bisect.sh.

2026-11-21: the ubu1 A/B's drop may be the scratch disk's: old-1 and the later sides wrote the same (io_write_bytes ~15.7 GB, ~700k writes), but their syncs took 3-4x as long (flush_sum_logs_us 12.6 s vs 34-48 s; durable_sum_files_us 2.7 s vs 12-20 s; raw_sync_max 149 ms vs 0.6-2.9 s): ubu1's scratch is ext4 on a ZFS zvol, which drifts up to 4x. The four-point bisect's rounds were on it too (882e2daf8 2737/2811/2995, e21e4efc4 2813/2980/2800): stopped. Rerunning de9638df vs 21ca0aeb7 with each run's stores on ext4 over a RAM-backed loop device inside its bench job (new: scripts/ab-guest-cases.sh CLADEFS_AB_RAM_DISK).
<!-- SECTION:NOTES:END -->
