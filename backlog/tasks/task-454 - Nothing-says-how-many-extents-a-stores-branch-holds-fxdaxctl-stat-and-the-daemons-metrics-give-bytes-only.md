---
id: TASK-454
title: >-
  Nothing says how many extents a store's branch holds: cladefsctl stat and the
  daemon's metrics give bytes only
status: To Do
assignee: []
created_date: '2026-11-15 13:15'
updated_date: '2026-11-15 14:51'
labels:
  - perf
  - review-2026-11-15
dependencies: []
references:
  - TASK-443.2
priority: medium
ordinal: 850000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-443.2 found that daemon heap per open fork is ~220-470 B times the base's extent count, so the count of a real sandbox base decides whether that is megabytes or tens of gigabytes; and no command, stat or metric gives it. cladefsctl stat prints capacity, used, available, own, inherited, public (bytes); extent_store::Store::extents() has it but nothing exposes it. Without it the size of TASK-443.2's problem on a real base can't be read from a host.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 cladefsctl stat DIR prints the number of extents the store's branch holds, own and inherited
- [ ] #2 The count of a real sandbox base image on ubu1 is recorded in TASK-443.2's notes
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
