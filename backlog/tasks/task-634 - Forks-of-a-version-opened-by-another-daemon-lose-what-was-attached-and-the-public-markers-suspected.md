---
id: TASK-634
title: >-
  Forks of a version opened by another daemon lose what was attached and the
  public markers (suspected)
status: To Do
assignee: []
created_date: '2026-11-18 17:04'
labels:
  - fork
  - public-base
dependencies: []
priority: medium
type: bug
ordinal: 940000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
A version's forks inherit what is attached to the store it was made of, and its public markers (default-arena sharing), only in the daemon process that made it. `opened_version` (crates/cladefs/src/cladefsd/cells.rs) calls `crate::cell_dir::inherit_attached(&of, &made)` only when `crate::host_cell::version_made_of(cell, version)` is Some, and `VERSIONS_MADE` (host_cell.rs) is an in-memory map of the versions this process made. So a fork of a public base's version on any other host, or on the same host after the daemon restarts, keeps no public marker (its pages go to the tenant's arena, one copy per tenant rather than one per host) and no attached bases. Found by the multi-region research (doc-23), not reproduced. Whether the VM manager is expected to call MakePublic or AttachBase again on each host is not documented (the proto and integrator-guide.md are silent).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A test forks a public base's version on a second daemon (and after a restart) and shows whether its pages are in the default arena
- [ ] #2 Either such forks inherit the markers and attachments wherever they are opened, or the proto and integrator-guide.md say what the VM manager must do on each host
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
