---
id: TASK-427
title: >-
  extent_store crash: recovers_from_its_bucket_with_and_without_its_disk
  acknowledges a commit in 2-4 of 100 rounds in the sim cargo profile (needs
  >25)
status: To Do
assignee: []
created_date: '2026-11-15 06:05'
labels:
  - test
  - task-407
dependencies: []
priority: low
ordinal: 818000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Run in the sim cargo profile (optimized with debug assertions; scripts/test.sh runs only sim:: tests there, so this is outside CI's configuration), extent_store's crash test recovers_from_its_bucket_with_and_without_its_disk fails its coverage check: '2 rounds acknowledged a commit', then '4 rounds' on a rerun (assert uploaded > KEPT_ROUNDS / 4 = 25). It passes in the dev profile (macOS gate 2026-11-15). Found 2026-11-15 after TASK-407 (a durable commit now acknowledges by PUT then the head's swap, two requests where it was one), so the crash may now land before most acknowledgements when the store runs fast. Find whether the dev profile's margin shrank too, and make the workload's crash point leave room for an acknowledgement in any profile, rather than lowering the bound.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
