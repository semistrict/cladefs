---
id: TASK-539.8
title: 'Admission inside the store: DiskFull with reserve and pledge'
status: To Do
assignee: []
created_date: '2026-11-18 14:16'
labels:
  - extentd
  - disk
dependencies:
  - TASK-539.4
  - TASK-539.7
documentation:
  - backlog/docs/doc-22 - Disk-governor-the-hosts-disk-as-a-managed-resource.md
parent_task_id: TASK-539
priority: high
type: feature
ordinal: 940000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
One admission point for every writer (VM socket, FUSE, image device, control-API makings), checked before bytes exist, so the store's own unrefusable work always fits. It also removes server.rs's three admit sites. (doc-22 task T7.)
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Footprint::admit(bytes) is called in LocalCell::begin_write_locked after state.check_room and in make_copy with STORE_MADE. The Write holds the Pledge until finish or Drop.
- [ ] #2 It checks, as in doc-22 §4: the HostDisk (using at most one band of unevicted cache), the allotment limit, the total limit, and the share under pressure. A refusal is local_cell::DiskFull, which open_cell::errno and FUSE map to ENOSPC, logged once per episode, and wakes the governor.
- [ ] #3 Tenant::admit_to_disk, disk::Admitted, its three server.rs call sites, OpenCell::disk_bytes and Usage::disk_bytes are deleted. The ENOSPC log prints the footprint.
- [ ] #4 With MemDisk::with_capacity, guests write until refused over many seeds: no store reaches Standing::Failed, and a commit succeeds while every write is refused.
- [ ] #5 hostile.rs and tests/disk.rs pass with refusals exactly at the limit, and the tenants-disks-unlimited mutant, retargeted to Footprint::admit, is killed.
- [ ] #6 RESERVE_FACTOR is the value T12 proved, not the placeholder 3
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
