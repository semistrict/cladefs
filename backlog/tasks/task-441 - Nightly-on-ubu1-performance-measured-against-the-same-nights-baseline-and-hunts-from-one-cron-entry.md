---
id: TASK-441
title: >-
  Nightly on ubu1: performance measured against the same night's baseline, and
  hunts, from one cron entry
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 11:46'
updated_date: '2026-11-15 23:48'
labels:
  - testing
  - ubu1
  - perf
dependencies:
  - TASK-375
  - TASK-368
  - TASK-397
priority: high
ordinal: 830000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Almost every performance problem so far was found by the sandbox team first (rounds 5-9; TASK-310, 311, 314, 317), because nothing measures main on a schedule: no workflow or schedule runs the perf gate, the fs bench or scripts/qualify.sh, and GitHub runners are too small and noisy for timings. The only hardware is ubu1 (16 threads, 27 GB, one NVMe), shared with agents at nearly every hour: its fxjob history of 2026-11-13..15 shows jobs in every local hour but 02:00-04:00, so the time of day cannot be the isolation. fxjob --bench isolates CPUs 4-7, L3, memory bandwidth and memory, one measurement at a time; it does not isolate the disk (the scratch disk is a zvol on the pool /home is on; work is capped on scratch alone). Decided with the user 2026-11-15: cron on ubu1; the measurement may hold the bench slot for whatever it needs; results go to a history page under docs/measurements and each metric that moved becomes a backlog task, pushed to main. Design agreed: skip a night main did not move; build as work, measure from prebuilt artifacts; compare HEAD against last night's commit in interleaved rounds of the same night, never against stored numbers; a control case with no cladefs in it (ext4 on virtio-blk) in every round, a round whose control moved discarded as the host's; rounds others wrote beside marked contaminated, rerun once, then reported as no result; counts (bytes written per guest byte, peak memory, requests, bucket calls) reported beside timings, as they do not skew with load.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A cron entry on ubu1, installed by a system-setup.d step, runs one script from the repo each night, and a night main did not move does nothing
- [ ] #2 A commit that slows a measured case by 10% (a mutant) is reported the next night; 5 nights of unchanged code report nothing
- [ ] #3 A night run beside a disk loader and a CPU loader as ordinary work reports no regression: its rounds are discarded or rerun, saying why
- [ ] #4 README's testing section says what runs nightly, where its results are and how to run a night by hand
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-15: installed on ubu1 by ~/docs/system-setup.d/151-cladefs_nightly.sh (committed in ubu1's ~/docs as 5a30bd1), which runs scripts/nightly-install.sh as ramon: clone ~/src/cladefs-nightly, cron 0 2 * * * nightly-host.sh, log ~/.local/state/cladefs/nightly/cron.log. Checked first: backlog CLI 1.55.3 in ~/.local/bin; a cron-like environment (env -i, no agent, BatchMode) can push to origin over ssh (key without passphrase; dry run). nightly-install.sh was mode 644 in git; fixed in 66655cd02. The first night is the one starting 02:00 EST 2026-11-16; nothing has run on Linux before it.
<!-- SECTION:NOTES:END -->
