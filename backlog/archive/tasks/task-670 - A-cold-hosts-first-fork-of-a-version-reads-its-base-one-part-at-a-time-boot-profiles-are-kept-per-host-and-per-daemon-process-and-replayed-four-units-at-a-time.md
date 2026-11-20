---
id: TASK-670
title: >-
  A cold host's first fork of a version reads its base one part at a time: boot
  profiles are kept per host and per daemon process, and replayed four units at
  a time
status: To Do
assignee: []
created_date: '2026-11-19 19:28'
updated_date: '2026-11-20 19:47'
labels:
  - performance
  - sandbox
  - cache
dependencies: []
priority: high
ordinal: 1053000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
fxfs-guest-python-sandbox (TASK-665, docs/measurements/ubu1-python-sandbox-2026-11-19T1842Z-7786f0b5) showed a sandbox on a cold host taking 13.4 s from fork to answers (10.2 s when its peers were warm), against 6.0 s warm. Host B made 164 ranged GETs of ~36 ms each (5.96 s of request time) and C 172 more serving B, within 7.4 s of extra wall time: about 1.6 requests in flight; with warm peers, 271 parts at ~15 ms each, one after another. The guest's DAX faults fetch what they touch, a part at a time, and Python's imports are scattered across many files, so object read-ahead finds no sequence to follow. TASK-38's boot profiles were meant to cover this, but a fork of a version records and reads its profile in the version's directory, host_cell::version_dir, which is in the daemon's temp directory and named by its pid: every host, and every daemon restart, starts without a profile, so the first fork on each pays the whole cold read (each host counted 2,169 prefetches, all of them in its second sandbox). Replay is also four threads reading one unit at a time, synchronously.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A version's boot profile is kept in the bucket with the version: the first fork of the version on any host to finish recording writes it, once (a later writer does not replace it), encrypted as the cell's other records are; the collector removes it with the version
- [ ] #2 A host's first fork of a version with no profile of its own reads the bucket's, and later forks there reuse it without asking the bucket again
- [ ] #3 Replay reads the profile's distinct parts, not its units, all at once as read-ahead under the host fetcher's budget (peers asked first, as for demand reads), so many parts are in flight together; a test shows more than four parts in flight during a replay
- [ ] #4 fxfs-guest-python-sandbox on ubu1: a cold host's first sandbox answers within 1.5x of a warm one, with cold and with warm peers, recorded in docs/measurements beside the first run
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Superseded 2026-11-20 by TASK-702 (round 11): the prediction of a version's reads is built from recent evidence objects written once beside the version (TASK-702.2) and prefetched into the part cache at open (TASK-702.3), not one profile written once and kept forever. Archived.
<!-- SECTION:NOTES:END -->
