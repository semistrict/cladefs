---
id: TASK-652
title: >-
  fxjob starts the next job before the last one's temporary directory is
  removed: a job that filled the scratch disk fails the next at its first mkdir
  (ENOSPC)
status: To Do
assignee: []
created_date: '2026-11-18T18:02:00Z'
labels:
  - fxjob
  - ubu1
dependencies: []
references:
  - scripts/fxjob
priority: medium
type: bug
ordinal: 937000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen twice on ubu1 on 2026-11-18 (TASK-537.3): a job waiting in the queue started the moment a 12G e2e job ended and died at once with "mkdir: No space left on device", the scratch disk full; a minute later it had 35G and then 111G free. fxjob frees a job's reservation when its command ends, and the job's EXIT trap (leave -> clean) removes its TMPDIR after; the waiting job polls every second and runs mkdir -m 1777 "$tmp_root/$$" as soon as the reservation fits, while the finished job's tens of GB are still being removed. Every agent's job queued behind a disk-filling one is lost this way, after waiting its turn.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A job starts only once the temporary directories of the jobs that ended before it are removed (or the next job waits for room on the scratch disk), so that it never fails at its own mkdir for space a finished job still holds
- [ ] #2 scripts/fxjob-check.sh shows it: a job filling the scratch disk, a job queued behind it starts and runs
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
