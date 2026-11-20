---
id: TASK-696
title: >-
  scripts/tests
  test_nightly.Room.test_a_night_short_of_room_before_building_builds_nothing
  flaked once: its seed's bare clone exited 128
status: To Do
assignee: []
created_date: '2026-11-20 14:15'
labels: []
dependencies: []
priority: low
ordinal: 1086000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Mac, 2026-11-20, scripts part of scripts/test.sh on PR #2 merged with main: subprocess git clone -q --bare <tmp>/seed <tmp>/origin.git returned 128 (stderr not shown by the test). Passed 3/3 alone after. The test should show git's stderr.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
