---
id: TASK-727
title: >-
  cladefsd refuses to start over a dead daemon's sockets (EEXIST on its control
  socket)
status: To Do
assignee: []
created_date: '2026-11-21 08:06'
labels:
  - daemon
dependencies: []
priority: medium
type: bug
ordinal: 1124000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
A daemon killed (SIGKILL, a crash) leaves its VM socket and its control socket (<socket>.control); a daemon started at the same socket fails: 'listening at …/host-a.control: File exists (os error 17)' (cladefsd/boot.rs, bind_owner_only). Found by TASK-702.3's sandbox harness restarting hosts; the harness now removes them. A host's daemon restarted after a crash would fail the same, unless its supervisor removes them. grants.rs has remove_stale for tenants' sockets, which removes a socket whatever its liveness.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A daemon starting at a socket a dead daemon left binds it; one a live daemon serves is refused (connect first: refused means stale), for the VM socket and the control socket alike (test)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 scripts/upstream-changes.py --check passes
- [ ] #2 Deviations from upstream documented where they occur
<!-- DOD:END -->
