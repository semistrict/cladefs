---
id: TASK-482
title: 'Cell volume → vdisk: the rename TASK-474 left for after cladefsd (TASK-475)'
status: To Do
assignee: []
created_date: '2026-11-16 04:50'
labels:
  - naming
dependencies: []
ordinal: 880000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
CONTEXT.md (2026-11-15, the user's choice): a cell's named branch is a vdisk; volume is Fxfs's alone. TASK-474 deferred it because TASK-475 (one daemon, cladefsd, serving FUSE cells; on another branch until it lands) is rewriting the same code: mount.proto, the FUSE mount, the store table. Start after TASK-475 is on main.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 cell::Volume → cell::Vdisk, its Kind, CreateVolume → CreateVdisk (mount.proto, extentd.proto VolumeKind), Volume DEK → vdisk DEK, DIR/TENANT/CELL/VOLUME → VDISK in paths and docs
- [ ] #2 volume names only Fxfs volumes in code and docs; CONTEXT.md's Ambiguities no longer says the code still says volume
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
