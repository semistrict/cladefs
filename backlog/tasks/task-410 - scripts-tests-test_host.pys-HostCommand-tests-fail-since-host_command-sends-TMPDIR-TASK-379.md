---
id: TASK-410
title: >-
  scripts/tests/test_host.py's HostCommand tests fail since host_command sends
  TMPDIR (TASK-379)
status: Done
assignee:
  - '@claude'
created_date: '2026-11-14 18:32'
updated_date: '2026-11-14 18:33'
labels:
  - scripts
  - bug
dependencies: []
priority: medium
ordinal: 798000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
679737d41 (TASK-379) made host_command pass TMPDIR="${TMPDIR:-/tmp}" through sudo env, but test_host.py's BASE was not updated: 8 HostCommand tests fail on macOS and Linux (seen on ubu1 run t39512h, scripts' tests FAILED (failures=8), each an extra 'TMPDIR=/tmp'). Blind spot: the scripts' tests weren't run before that push.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 test_host.py's HostCommand tests pass, BASE naming TMPDIR=/tmp
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
BASE names TMPDIR=/tmp; test_host 60/60 on macOS.
<!-- SECTION:FINAL_SUMMARY:END -->
