---
id: TASK-425
title: >-
  fxdax storage::image::tests::flipped_bytes_fail_reads hung for 53 min under
  the full macOS suite
status: To Do
assignee: []
created_date: '2026-11-15 05:08'
updated_date: '2026-11-15 05:09'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 819000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen 2026-11-15 on branch t3/gcp-kms-backend (TASK-411.18, whose changes don't touch this path: the test uses no KMS, extent_frame::NoKeys): cargo nextest run -p extent_keys -p extent_remote -p extent_store -p fxdax -E 'not (package(extent_store) & test(/^sim::/))' on the Mac (Apple M5 Pro) ran 961 of 962 tests to pass; this one went SLOW past 3180 s and was killed with SIGTERM at 3202 s, its only output 'has been running for over 60 seconds'. Run alone it passes in 0.35 s, 30 of 30 times. A hang only under load: something in the device's read of a part with a flipped byte, or the flush before, waits for what never comes when the machine is busy.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The hang is reproduced (under load, e.g. alongside the suite or a CPU hog) or its cause found from the code
- [ ] #2 Fixed so the test cannot hang; a run of it under load passes, or a stuck wait fails fast with what it waited for
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Perhaps TASK-414's cause (an image's collector pass never ending on macOS, its Collecting's drop joining it for ever): this test's device is an image too.
<!-- SECTION:NOTES:END -->
