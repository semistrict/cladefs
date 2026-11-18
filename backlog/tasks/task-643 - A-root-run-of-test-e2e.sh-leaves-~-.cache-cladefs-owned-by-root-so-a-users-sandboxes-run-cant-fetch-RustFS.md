---
id: TASK-643
title: >-
  A root run of test-e2e.sh leaves ~/.cache/cladefs owned by root, so a user's
  sandboxes run can't fetch RustFS
status: To Do
assignee: []
created_date: '2026-11-18 20:22'
labels:
  - testing
dependencies: []
priority: medium
type: bug
ordinal: 1029000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
On ubu1 (found 2026-11-18 by TASK-632's framed-in-copies kill), ~/.cache/cladefs was root:root since 2026-11-15 18:03, holding docker-29.9.1 and downloads: a run as root with HOME kept (sudo) made it with test-e2e.sh's docker download (${XDG_CACHE_HOME:-$HOME/.cache}/cladefs/docker-VERSION). A later scripts/sandboxes.sh short as the user then failed setup, exit 2, 'mkdir: Permission denied' with no path: scripts/lib/rustfs.sh's rustfs_binary couldn't make cladefs/rustfs-VERSION-PLATFORM there. Fixed on ubu1 by hand (chown). The caches a root run makes in the user's home should be the user's (or the scripts say which directory they couldn't make).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A run of test-e2e.sh as root leaves no directory under the invoking user's ~/.cache that the user can't write
- [ ] #2 rustfs_binary's failure names the directory it couldn't make
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
