---
id: TASK-502
title: >-
  firecracker-series-check.sh fails the series' commits before TASK-417: they
  build against today's cladefs crate, whose TASK-474 renames they predate
status: To Do
assignee: []
created_date: '2026-11-16 23:16'
labels:
  - firecracker
  - ci
dependencies: []
ordinal: 891000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
firecracker-series-check.sh builds every commit of the Firecracker series with the cladefs feature against the checkout's own crates/cladefs. The commits before the device moved onto crates/virtio_extent (TASK-417) call cladefs's API as it was then: cladefs::RemoteExtentStore, cladefs::Commit::Durable, a request tuple whose last element was sized. TASK-474 renamed those (daemon client, Commit::Uploaded/Local), so those 8 commits fail clippy (E0432, E0599, E0277), first seen when TASK-501 moved vendor/firecracker (CI run 43974684702, reproduced on ubu1: ~/src/fxdax-t501-series/1a81da3cf.log). Main never noticed: CI runs the job only when vendor/firecracker moves. The series' tip and the commits after TASK-417 pass both ways.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The series check passes on a move of vendor/firecracker: the old commits either build against the cladefs they were written for, or the check holds them to the build without the feature alone
- [ ] #2 CI's firecracker-series job also runs when crates/cladefs or crates/virtio_extent change in a way the series uses, or the check says why not
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
