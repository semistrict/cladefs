---
id: TASK-723
title: >-
  On asahimbp (arm64, 16K pages) a memory window's first fault fails EINVAL and
  aborts the test: memory_windows_map_what_faults_reach_and_share_their_pages
status: To Do
assignee: []
created_date: '2026-11-21 07:39'
updated_date: '2026-11-21 07:39'
labels:
  - bug
  - arm64
dependencies: []
priority: medium
ordinal: 1128000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen 2026-11-21 in the podman image on asahimbp (rootless, label=disable, seccomp=unconfined, /dev/userfaultfd passed with keep-groups), on the pre-async base 1b3b5a991 too, so not the async rewrite's. cladefsd::tests::memory_windows_map_what_faults_reach_and_share_their_pages aborts (SIGABRT): request 32 refused with EINVAL (flags 0x0, args all 0), then 'cladefs::memory_cell: mapping node 0 of a memory's window: EINVAL', 'unresolved DAX window fault at offset 0x0 (Read): EINVAL'. Likely 16K pages (a 4K assumption in a memory window's node mapping) or the container. ubu1 (x86-64, 4K) passes it.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 scripts/upstream-changes.py --check passes
- [ ] #2 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Correction: 'ubu1 passes it' is unverified as of filing; ubu1's unit runs were green before 2026-11-21, not checked for this test since.
<!-- SECTION:NOTES:END -->
