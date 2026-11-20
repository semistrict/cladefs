---
id: TASK-708
title: >-
  cladefsd makes its replica directory, ephemeral directory and tenants
  directory without syncing their parents
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-20 21:33'
updated_date: '2026-11-20 21:34'
labels:
  - bug
  - replication
  - durability
dependencies: []
priority: high
ordinal: 1112000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the madsim replicated partitions profile once each simulated host's disk crashes at its restart (TASK-685.4, seeds 62, 116, 129, 143, 202 of 300). peers::replicas made CLADEFSD_REPLICAS with create_dir_all, its entry in its parent never synced; a crash soon after the first boot can lose the whole directory, its incarnation file with it, though that file was written and synced: the member then makes a new incarnation and refuses (Aborted) every take fencing the configuration that named the old one, and with the writer's host down the cell can't be taken (EIO, 200 tries). The same for ephemeral_host's cells directory and boot's tenants directory. Fix: host_fs::create_dir_all_durably, each directory it makes synced in its parent, for those three (and the simulation's host roots, as a host's root is durable before its daemon).
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
