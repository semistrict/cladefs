---
id: TASK-720
title: >-
  fxjob on asahimbp: a second host, measurements (--bench) isolated on its
  P-cores
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-21 06:34'
labels:
  - ubu1
  - asahimbp
dependencies: []
priority: medium
ordinal: 1124000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The user added asahimbp (MacBook Pro, Fedora Asahi Remix 45, arm64, 11 CPUs: P-cores 0,7-10 in one cluster and cpufreq policy, E-cores 1-6; 17 GB; 16K pages; btrfs only) beside ubu1 and wants fxjob there, measurements included (2026-11-21). fxjob and its isolation were written for ubu1 alone: cores 4-7,12-15 hard-coded in scripts/fxjob and in fxjob-isolate, which lives only as a heredoc in ubu1:~/docs/system-setup.d/146-fxdax_cgroups.sh and also sets AMD L3 CAT and MBA (no resctrl on Apple Silicon) and caps the other cores at their ACPI nominal clock (a cluster shares one policy on Apple, so capping cpu0 would cap the measurement). A probe on 2026-11-21 (single-threaded loop on cpu7, 8 runs): cpu0 busy +2% median, E-cores busy +1%, four other P-cores busy +7%: work on the cluster's spare P-core costs a measurement. asahimbp's own setup is ~/setup/scripts/NN-*.sh there (not a git repo). asahimbp has no ext4 for the scratch disk work's IO is capped on.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 scripts/fxjob takes a measurement's CPUs from /etc/fxjob.conf, not from code; a host without them refuses --bench
- [ ] #2 fxjob-isolate is in the repo, one source for both hosts, its L3/MBA and clock parts used only where the host's conf asks for them
- [ ] #3 On asahimbp, fxjob --bench runs a job in a cpuset partition of P-cores 7-10, work kept off the P-cluster (on E-cores 1-6) while it runs, and everything restored after
- [ ] #4 asahimbp has /mnt/fxdax-scratch (ext4) and work's IO there is capped while a measurement runs
- [ ] #5 scripts/fxjob-check.sh passes on asahimbp
- [ ] #6 ubu1's step 146 installs the same fxjob-isolate and a conf naming its CPUs, cache and clocks as today
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 scripts/upstream-changes.py --check passes
- [ ] #2 Deviations from upstream documented where they occur
<!-- DOD:END -->
