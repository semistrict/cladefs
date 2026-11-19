---
id: TASK-656
title: >-
  fxjob: a measurement's MemoryMin stays on fxdax.slice after it ends,
  sheltering work's page cache from reclaim, so ubu1 OOM-kills new jobs with 13
  GB available
status: To Do
assignee: []
created_date: '2026-11-19 10:47'
updated_date: '2026-11-19 13:41'
labels:
  - fxjob
  - ubu1
dependencies: []
priority: high
type: bug
ordinal: 1037000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen 2026-11-19 on ubu1, no job running: fxdax.slice, its bench slice, user@1000.service, user-1000.slice and user.slice all had memory.min 16 GiB (scripts/fxjob sets -p MemoryMin="$mem" on a measurement's scope; systemd raises the ancestors' to cover it, and they keep it once the scope is gone). fxdax-work.slice held 14.7 GiB of page cache (file 15.2 GiB, anon 0) left by earlier jobs; with the bench slice using 670 MiB, the protection it no longer needs covers work's cache through recursive protection. A new job's rust-lld was killed by a global OOM (dmesg: global_oom, "Memory cgroup min protection 16621192kB", task_memcg fxjob-b2-fork-*.scope, minio invoked oom-killer) four times in a row with free showing 13 GiB available and 15 GiB buff/cache; fxdax.slice memory.events had counted 82 oom_kills. echo 12G > fxdax-work.slice/memory.reclaim freed it (14049M -> 1199M) and the next job compiled. Fix: the measurement's MemoryMin goes with the measurement (reset the slices' memory.min, or put it on the scope alone with the ancestors' left as they were), and work's cache is reclaimable whatever ran before.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Once a measurement ends, fxdax.slice and its ancestors' memory.min are back to what they were before it
- [ ] #2 A job compiling cladefs after a measurement, with work's slice holding page cache, is not OOM-killed while the host has the memory available
- [ ] #3 scripts/fxjob-check.sh checks the first
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-19: memory.reclaim alone did not hold: a job's own build refilled work's cache (13.6 GiB) and was OOM-killed linking. With no measurement running, reset at runtime (systemctl set-property --runtime ... MemoryMin=0) on fxdax-bench.slice, fxdax.slice (user manager) and user@1000.service, user-1000.slice, user.slice (sudo): all 0 now. The next measurement sets its own again, which this task's fix must take back when it ends.
<!-- SECTION:NOTES:END -->
