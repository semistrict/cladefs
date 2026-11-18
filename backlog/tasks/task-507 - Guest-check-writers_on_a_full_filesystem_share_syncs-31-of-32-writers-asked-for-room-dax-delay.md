---
id: TASK-507
title: >-
  Guest check writers_on_a_full_filesystem_share_syncs: 31 of 32 writers asked
  for room (dax-delay)
status: To Do
assignee: []
created_date: '2026-11-17 03:44'
labels:
  - bug
  - guest
dependencies: []
ordinal: 893000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
ubu1 run r504b32 (commit 3ad2bd165, TASK-504, which touches no guest code), case fxfs-guest-checks-dax-delay, seed 1786761369942784109: "31 writes asked for room, of 32 writers" (attributes.rs: asked >= WRITERS). One writer of 32 never asked the filesystem for room: room another freed, or a write that went through without finding the filesystem full. The assertion presumes every writer hits ENOSPC once; under the dax-delay configuration one did not. Either the check is wrong about what full means (room freed between fill and writes, by a sync another writer started), or make_room counts a write that found no room twice or not at all. Same check failed once on GCE r941 (TASK-149.25 notes).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Root cause named: why a writer on a full filesystem does not ask for room
- [ ] #2 The check passes 50 repeats of fxfs-guest-checks-dax-delay with CLADEFS_CHECKS=writers_on_a_full_filesystem_share_syncs, or the guest is fixed and the check kept as strict
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
