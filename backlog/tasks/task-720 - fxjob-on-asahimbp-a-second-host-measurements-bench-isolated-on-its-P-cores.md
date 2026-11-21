---
id: TASK-720
title: >-
  fxjob on asahimbp: a second host, measurements (--bench) isolated on its
  P-cores
status: Done
assignee:
  - '@claude'
created_date: '2026-11-21 06:34'
updated_date: '2026-11-21 07:37'
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
- [x] #1 scripts/fxjob takes a measurement's CPUs from /etc/fxjob.conf, not from code; a host without them refuses --bench
- [x] #2 asahimbp has /mnt/fxdax-scratch (ext4) and work's IO there is capped while a measurement runs
- [x] #3 scripts/fxjob-check.sh passes on asahimbp
- [x] #4 ubu1's step 146 installs the same fxjob-isolate and a conf naming its CPUs, cache and clocks as today
- [x] #5 On asahimbp, fxjob --bench runs a job in a cpuset partition of P-cores 7-10, work, workqueues and movable IRQs on CPUs 0-6 while it runs, and everything restored after
- [x] #6 fxjob-isolate has one source for both hosts, its L3/MBA and clock parts used only where the host's conf asks for them
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 scripts/upstream-changes.py --check passes
- [x] #2 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. fxjob reads a measurement's CPUs from /etc/fxjob.conf (BENCH_CPUS); fxjob-isolate one source, its L3/MBA/clock parts from the conf. 2. asahimbp setup: ext4 scratch loop image, slices, budget, conf BENCH_CPUS=7-10, podman split cgroups. 3. ubu1 step 146: conf with today's isolation, install the shared isolate. 4. Verify: fxjob-check on both hosts, a real --bench on each. (Later, by the user's word: fxjob moved to the local fleet skill, installed by its deploy.sh.)
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Design change from the AC as filed: work keeps cpu0 (a P-core of the measurement's cluster) while a measurement runs. Probe 2026-11-21, a loop on cpu7, median of 8: cpu0 busy +2%, E-cores busy +1%, the other P-cores +7%: an idle cpu0 would buy ~1% for 1 of 11 CPUs. AC #3 reworded to that.
fxjob fixes found on asahimbp: fxjob ps errored when a slice's io controller was off (io.max absent); the history read the measure shell's own cgroup, which podman (cgroups=split) moves into the scope's runtime child (now the scope's); reap printed tee's error when the scope ended as it was killed. fxjob sets CONTAINERS_CONF_OVERRIDE=/etc/fxjob-containers.conf where the host has it, so rootless containers run in the job's scope: a container reading 3G in a --mem 512M job was killed (137, peak 512M + 2G swap).
Verified on asahimbp: fxjob-check.sh all passed; a real --bench: job Cpus_allowed 7-10, partition root, work 0-6, workqueue/IRQ mask 07f, work io.max on loop0, all undone after. fxjob-check.sh all passed on ubu1 with the repo copy.

User, 2026-11-21: host details and fxjob moved out of the repo into the user's local fleet skill (~/.claude/skills/fleet, with fxjob/deploy.sh); scripts/fxjob, fxjob-isolate and fxjob-check.sh removed from the repo; both hosts' setup steps install from what deploy.sh copies to ~/.local/share/fxjob. Deployed to both; fxjob-check passed on both from the deployed copies.

ubu1 verified with a real fxjob --bench (queued 25 min behind others): job Cpus_allowed 4-7,12-15, partition root, work cpus 0-3,8-11, workqueue/IRQ 0f0f, work io.max on the scratch zvol, resctrl bench L3 fff0 MB 2048, root L3 000f MB 40, cpu0 capped at nominal 4001000; after: workqueue/IRQ ffff, root L3 ffff MB 2048, bench group gone, cpu0 back to 5263061, no saved state left. Same isolation as before the change. DoD: scripts/upstream-changes.py --check passes; no upstream code touched.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
fxjob runs on a second host, asahimbp, measurements included: a measurement's CPUs, cache, bandwidth and clocks come from each host's /etc/fxjob.conf instead of code, so one fxjob and one fxjob-isolate serve ubu1 (AMD: partition, L3 CAT, MBA, clock caps) and asahimbp (Apple Silicon: P-cores 7-10 partition only). asahimbp got an ext4 scratch disk, slices and budget (12G, 11 CPUs), and rootless podman containers run inside the job scope so --mem and reap reach them. Found and fixed on the way: fxjob ps errored with a slice's io controller off; history read the wrong cgroup under podman; a spurious tee error on reap. At the user's request fxjob then moved out of the repo into the user's local fleet skill with a deploy script. Verified: fxjob-check.sh all passed on both hosts; a real --bench on each isolated as specified and restored everything after; a podman job past its --mem was killed (137).
<!-- SECTION:FINAL_SUMMARY:END -->
