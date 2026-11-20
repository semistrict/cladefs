---
id: TASK-698
title: >-
  Ephemeral cells: host-local children of a bucket version, surviving restarts,
  migrated live (doc-25)
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-20 16:20'
updated_date: '2026-11-20 16:30'
labels: []
dependencies: []
documentation:
  - >-
    backlog/docs/doc-25 -
    Ephemeral-cells-host-local-children-of-a-version-migrated-live.md
priority: high
ordinal: 1089000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The sandboxes run a local mode today: overlayfs in the guest, with a JuiceFS snapshot as its lower layer and a virtio block device on the host NVMe as its upper. cladefs has no equivalent, because every cell is uploaded and its sync/syncfs wait for the bucket. The user chose the shape on 2026-11-20 (doc-25): an ephemeral cell forked from a version, kept raw on one host disk, never uploaded or promoted, with no versions of its own. It survives a daemon restart or crash and a host reboot, and moves between hosts by live migration (pre-copy). The parent version is kept by a reference naming the holding host incarnation.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A VM boots a fork of a bucket version as an ephemeral cell, and nothing of the cell own data reaches any bucket
- [ ] #2 An ephemeral cell survives a daemon crash and restart, and a drain, with every Local commit intact
- [ ] #3 A running VM on an ephemeral cell migrates live to another host with no Local commit lost, its downtime measured on the test host
- [ ] #4 doc-25 decisions hold in code, spec and simulation; CONTEXT.md defines the new terms
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
