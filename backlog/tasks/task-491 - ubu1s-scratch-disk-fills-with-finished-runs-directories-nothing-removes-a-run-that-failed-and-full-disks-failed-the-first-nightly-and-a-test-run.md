---
id: TASK-491
title: >-
  ubu1's scratch disk fills with finished runs' directories: nothing removes a
  run that failed, and full disks failed the first nightly and a test run
status: To Do
assignee: []
created_date: '2026-11-16 09:37'
labels:
  - ubu1
  - bug
dependencies: []
priority: high
ordinal: 883000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
On 2026-11-16 /mnt/fxdax-scratch (251 GB, shared by every fxjob job and host run on ubu1) reached 100% twice in a few hours: the nightly's first night could not measure (TASK-489), and a clippy/workspace run of linux-batch failed `No space left on device` writing extent_store's rmeta. At 04:37 EST it held run-r4441j-20261115T152449Z-a43e935e (72 GB, job status 1, untouched since 2026-11-15 11:23, no process) and run-t474d-... (75 GB, being fetched), t435 (60 GB, a delegate's target and results, untouched since 00:15), t411f (13 GB). I removed r4441j by hand (147 GB free after). scripts/lib/host.sh / host-remote's clean removes a run's directory once its results are fetched, but a run that failed or whose driver died keeps its directory for ever (TASK-372 is about what fetch copies, TASK-367 a clean racing a cancel); agents' own scratch directories (t435, t411f) have no owner-side cleanup at all. TASK-489 made the nightly sweep its own runs and check room; nothing does so host-wide.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Runs under /mnt/fxdax-scratch/fxdax-host whose job ended (status written, no process in their scope) are removed after a stated grace (e.g. 6 h) whether they passed or failed, by something that runs on its own (a timer or each host run's start), with a test
- [ ] #2 fxjob refuses to start a job, saying why, when the scratch disk is under a stated floor of free space, with a test
- [ ] #3 Agents' own scratch directories are covered by a documented rule in AGENTS.md (where they go, who removes them, when)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
