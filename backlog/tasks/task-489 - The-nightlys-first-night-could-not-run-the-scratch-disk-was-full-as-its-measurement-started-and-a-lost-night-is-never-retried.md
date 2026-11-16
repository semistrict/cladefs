---
id: TASK-489
title: >-
  The nightly's first night could not run: the scratch disk was full as its
  measurement started, and a lost night is never retried
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-16 08:37'
updated_date: '2026-11-16 09:04'
labels:
  - bug
  - testing
  - ubu1
dependencies: []
priority: high
ordinal: 881000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
First night on ubu1, 2026-11-16 (docs/measurements/ubu1-nightly-2026-11-16T0700Z-7f4923f3): both sides built as work (02:00-03:33 EST), then the measurement job started in the bench slot and failed at once: remote.log says `mkdir: No space left on device` and `job: line 32: echo: write error: No space left on device` (the job could not write its status), then scripts/lib/host.sh host_wait failed with `return: : numeric argument required` (it returns "${said##*status }", empty when the status was never written). The night was reported "could not run: no round ran (the A/B ended 2)" and pushed. /mnt/fxdax-scratch (251 GB, the zvol all fxjob jobs share) was full at 08:33 UTC; an hour later it had 75 GB free, other agents' runs (t435 60 GB, t411f 13 GB) and the nightly's own fxdax-host run directories (83 GB) holding most of it. Three gaps: (1) nothing checks room before a night builds or measures, and the night does not say why it could not run (disk full); (2) host_wait turns a missing status into a bash error instead of a clear "status lost"; (3) nightly.py plans from the newest night's head whether or not that night measured anything, so a night that could not run is not retried the next night unless main's code moves.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A night checks the scratch disk's free space before building and before measuring, against what it needs (its run directories' sizes from earlier nights), and a night short of room says so in its report and log, naming the disk and what it found
- [x] #2 host_wait reports a job whose status is empty or unreadable as lost, with a test that fails before
- [x] #3 A night that could not run, or whose rounds were all discarded, does not count as measured: the next night measures the same head again, with a test
- [x] #4 The nightly's own run directories from earlier nights are removed but for the artifacts the next night reuses, with a test
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. host-remote.sh: wait prints `lost` for a status file missing, empty or not a number (`lost unreadable`); host_wait says why. Test (test_host Resume): an empty status is lost, red before (bash's 'numeric argument required').
2. host-remote.sh actions room DIR (free bytes and mount point, df -Pk) and size DIR (sudo du -sk), sweep SCRATCH REPO NAME (each run-NAME-<time>-<mark> dir, exact pattern, whose job is not running, removed by clean). host.sh: CLADEFS_HOST_ROOM (bytes, checked in host_start before prepare) and CLADEFS_HOST_ROOM_MEASURE (before a measurement's measure job), each check a line of RESULTS/room.txt (PHASE FREE NEED ok|short DISK), short fails 28 saying disk/free/need; the run directory's size after the build, `built BYTES`; CLADEFS_HOST_SWEEP=1 sweeps the earlier runs of CLADEFS_HOST_RUN's name first. Fake df/du in fake_ssh's host tools (controlled by files in the host's home). Tests in test_host.
3. bench-ab-linux-host.sh: ab_tidy records RESULTS/rounds-size.txt (the rounds' results with their stores, before they go) then removes the stores; tested sourced.
4. nightly.py: plan bases on the newest night that measured (outcome measured): skip only if it measured main's code; base its head, else the first night's base, else head^. room subcommand: needs = max(setting, 1.2 x largest of the last 7 nights): measure CLADEFS_NIGHTLY_ROOM_MEASURE (20G) vs rounds-size; build CLADEFS_NIGHTLY_ROOM_BUILD (100G, the first night's 83 GB of run directories and a fifth) vs built, plus measure. report: room.txt into night.json (room), a short check is could-not-run naming disk, free and need; nightly.md section on the scratch disk.
5. nightly-host.sh: room needs passed, CLADEFS_HOST_SWEEP=1, CLADEFS_AB_KEEP_COUNT 2 (tonight's two sides: all the next night reuses, retry included). Docs: headers, README.
6. test_nightly: a night that could not run is measured again the next night (same base, head); a night short of room before measuring (disk filled during the build) reports disk/free/need; the second night's need learned from the first's sizes; earlier nights' run dirs swept, others' runs, a running nightly run, look-alike names and the kept artifacts left.
7. Full unittest suite to a log, test.sh scripts part, shellcheck.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented (macOS; no ubu1 access). host-remote.sh: wait says `lost unreadable` for a status file empty or not a number; actions sweep/room/size. host.sh: CLADEFS_HOST_ROOM (checked in host_start before prepare) and CLADEFS_HOST_ROOM_MEASURE (host_job, between build and measure), RESULTS/room.txt lines `PHASE FREE NEED ok|short DISK` and `built BYTES` (run dir after the build, sudo du), short ends 28; CLADEFS_HOST_SWEEP=1 removes run-NAME-<time>-<mark> dirs (exact pattern, CLADEFS_HOST_RUN's name) whose job isn't running. bench-ab-linux-host.sh: ab_tidy writes RESULTS/rounds-size.txt before removing stores. nightly.py: plan from the newest night with outcome measured (skip only if it measured main's code; base its head); room = max(setting, 6/5 of the most of the last 7 nights), build adds measure; defaults CLADEFS_NIGHTLY_ROOM_BUILD 100G (83 GB of nightly run dirs after the first night, +1/5), CLADEFS_NIGHTLY_ROOM_MEASURE 20G (a guess until rounds-size.txt says); a short check is could-not-run naming disk, free, need; nightly.md section 'The scratch disk'. nightly-host.sh: room passed, sweep on, CLADEFS_AB_KEEP_COUNT 2 (tonight's sides: all the next night reuses), results dir to the second and made with mkdir (a retried night in the same minute shared the earlier's dir in tests). fake_ssh: df/du fakes driven by files in the home. README's nightly section updated.

Validation (macOS): CLADEFS_TEST_PARTS=scripts scripts/test.sh exit 0 (python3 -m unittest discover -s scripts/tests: 513 tests OK in 201 s; exclusions, license headers, mutant checks, firecracker-cargo --check). Every new test red against the unchanged scripts (a worktree of HEAD with the new tests: 10 failures, 3 errors; the retry test recorded one night of four, the old plan skipping the rest). shellcheck: no finding added on any touched shell file (bench-ab-linux-host.sh's SC2154/SC1091 are as before). AC #1: test_host Room.test_a_run_short_of_room_prepares_nothing, test_a_measurement_whose_build_filled_the_disk_measures_nothing; test_nightly Room.test_a_night_short_of_room_before_measuring_says_so (night.json, nightly.md, nightly.log), test_a_night_short_of_room_before_building_builds_nothing, test_the_need_follows_what_earlier_nights_took. AC #2: test_host Resume.test_a_job_whose_status_is_empty_is_lost. AC #3: test_nightly Nights.test_a_night_that_measured_nothing_is_measured_again. AC #4: test_host Room.test_the_earlier_runs_of_its_name_go_but_a_running_one_and_others, test_nightly Room.test_earlier_nights_runs_go_but_what_others_and_the_next_night_use, test_bench_ab_host Kept.test_only_the_artifacts_used_last_are_kept (count 2, the nightly's default now). Open: DoD #1 (no Rust changed; Linux suite needs ubu1) and the next real night on ubu1 (02:00 EST), the end-to-end check.
<!-- SECTION:NOTES:END -->
