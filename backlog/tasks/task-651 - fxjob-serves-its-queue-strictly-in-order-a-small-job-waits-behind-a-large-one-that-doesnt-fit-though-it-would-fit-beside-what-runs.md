---
id: TASK-651
title: >-
  fxjob serves its queue strictly in order: a small job waits behind a large one
  that doesn't fit, though it would fit beside what runs
status: To Do
assignee: []
created_date: '2026-11-18T19:13:00Z'
updated_date: '2026-11-19 06:41'
labels:
  - fxjob
  - ubu1
dependencies: []
references:
  - scripts/fxjob
  - TASK-652
priority: medium
type: enhancement
ordinal: 938000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
On ubu1 on 2026-11-18 (TASK-537.3) a clippy of three crates, 51 s of work, waited 1,067 s: a 14G job ran, a 10G job headed the queue (not fitting beside it in the 22G budget), and every job behind it waited too, whatever it asked for. With many agents submitting 10-14G e2e runs, every short job (a check, a clippy, a few tests, a TLC model) waits for whole e2e runs; TLA+ batches waited 5-19 min each the same day. Backfilling, as Slurm does, starts a job behind the head when it fits in what is free and cannot delay the head: here the head waits for memory a running job holds, so a job that fits now and ends first delays nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A job behind the head of the queue starts at once if it fits in the free budget and starting it cannot delay the head (by a bound on its run time, or by never letting it hold what the head needs once that is free)
- [ ] #2 Measurements (--bench) keep their place and isolation; no job starves
- [ ] #3 scripts/fxjob-check.sh shows a small job starting beside a running large one while a large one waits
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
