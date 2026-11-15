---
id: TASK-447
title: >-
  Sandboxes short mode on GitHub's runner: the builder VMs' docker build round
  takes 149 s on one runner and 457-561 s on others, the host's CPUs all in
  guest time, no IO
status: To Do
assignee: []
created_date: '2026-11-15 12:53'
updated_date: '2026-11-15 12:54'
labels:
  - perf
  - testing
  - ci
dependencies: []
references:
  - .github/workflows/sandboxes.yml
priority: medium
type: bug
ordinal: 829000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found in TASK-330.1's three sandboxes.yml short-mode runs on main bfdecbc13 (2026-11-15). The same code ran on three runners at once: 43709072839, 43709074493 and 43709076157. Every check passed on all three. But the two builder VMs (8 vCPUs each, DAX root, fxfs-guest-docker-build-dax's guest) took 153/157 s, 457/461 s and 573/577 s. Their 'docker build: round 1' took 149.5 s, 445-452 s and 560.8 s. In 43709076157, the builder's guest console shows overlayfs messages up to 63.7 s, then nothing until 560.8 s. Through that gap, resources.log puts the 4-CPU host at about 1% idle, 0% iowait and no dirty pages, load 11-12, its CPU all user/guest time. The daemons' logs say nothing slow then: framings stay under 1 s and flushes mostly under 0.6 s. The other roles were slower too, but by less: git 27 -> 79 s, pgbench 99 -> 137 s. The sandboxes' commands stayed fast (slowest 485/167/168 ms). The runners differ: the guest kernel build took 9:33 / 11:17 / 13:31 and the artifacts' cargo build 9:09 / 10:43 / 12:27, so the slow runners are 17-40% slower at compiling, but the builders were 3-3.7x slower. A slowdown out of proportion to the CPU, on 28 vCPUs over 4 CPUs, looks like the 8-vCPU guests spinning (lock-holder preemption: compare TASK-91.14's __pv_queued_spin_lock_slowpath) or the builders retrying DAX faults. Each builder's VMM log stops at its first 1001 'restricted to reads for a worker's write' lines (a log cap), so the logs can't tell which. The cost: the sandboxes step runs 3-10 min, and ci.yml's sandboxes job runs past the short mode's 10 min target on a slow runner.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The cause of the builders' 3-3.7x spread is measured on a slow GitHub runner (guest perf, or the VMM's fault counts over time), not guessed
- [ ] #2 The builder round is within 1.5x of the fast runner's on runners that compile within 1.4x of it, or the spread is shown to be the runner's alone and the case's budget says so
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
