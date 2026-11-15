---
id: TASK-409
title: >-
  specs/bucket's BucketTakeovers takes about 1h47m in TLC: split it into models
  under 10 minutes
status: To Do
assignee: []
created_date: '2026-11-14 17:37'
updated_date: '2026-11-14 23:56'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 797000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
BucketTakeovers is the one specs/bucket model scripts/check-tla.sh can't check in a short loop: 207 million states, 1 h 47 min on the Mac's 14 cores (its configuration, 2026-11-12). Models should take under 10 minutes, 20 at most. Split it by what it explores (writers taking over, late fences, segments in flight) into models that together cover what it covers, and argue that coverage in the configurations' comments. Each mutation it catches today must still be caught by one of them.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Every model that replaces BucketTakeovers checks in under 10 minutes on the Mac
- [ ] #2 The configurations argue that together they cover BucketTakeovers' behaviours
- [ ] #3 Every mutation BucketTakeovers caught is caught by one of them
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-14, TASK-407's head spec (branch t3/tombstones): BucketTakeovers holds in 575 s, 5,499,069 states (was 207M, 1 h 47 min): under 10 min with no split. Its writers' fences, strays and pins, what multiplied its states, are gone with the head. To close once TASK-407 lands.
<!-- SECTION:NOTES:END -->
