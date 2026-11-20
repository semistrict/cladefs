---
id: TASK-685.16
title: >-
  The full-stack simulation runs on macOS: cladefsd builds under --cfg madsim
  there
status: To Do
assignee: []
created_date: '2026-11-20 20:33'
labels:
  - testing
  - simulation
dependencies: []
parent_task_id: TASK-685
priority: medium
ordinal: 1110000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/cladefs/tests/madsim.rs is cfg(all(madsim, target_os = "linux")) because cladefs::cladefsd is Linux-only (lib.rs). Under madsim the daemon's Linux-only edges are already outside the simulation or simulated: the VM socket (guests in memory), SCM_RIGHTS, peer credentials, FUSE, metrics; physmem and local_cell/bucket_cell have macOS paths, and with TASK-685.4 each host's disk is a MemDir. Survey: about 5 Linux-only libc uses in cladefsd/server.rs plus whatever module gates hold; gate those to cfg(not(madsim)) or give macOS equivalents, build cladefsd under madsim on macOS, drop the target_os gate from the test, and run the madsim part of scripts/test.sh on the Mac. Gains: the simulation runs where TLC already runs (faster than ubu1, no queue).
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
