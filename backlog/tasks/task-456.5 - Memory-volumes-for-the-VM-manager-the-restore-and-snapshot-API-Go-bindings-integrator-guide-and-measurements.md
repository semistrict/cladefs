---
id: TASK-456.5
title: >-
  Memory volumes for the VM manager: the restore and snapshot API, Go bindings,
  integrator guide, and measurements
status: To Do
assignee: []
created_date: '2026-11-15 13:33'
labels:
  - memory
  - docs
dependencies:
  - TASK-456.4
parent_task_id: TASK-456
priority: medium
ordinal: 866000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
What a VM manager calls to snapshot a VM into its cell and restore it from there, and what it costs, against Firecracker's memory file.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 docs/integrator-guide.md describes snapshotting into and restoring from a cell's memory volume; the Go bindings carry the calls
- [ ] #2 Measured on ubu1 under fxjob --bench: snapshot and restore time of 4 GiB and 64 GiB VMs, bytes stored, and host memory for 1, 10 and 100 VMs restored from one snapshot, against Firecracker's File backend
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
