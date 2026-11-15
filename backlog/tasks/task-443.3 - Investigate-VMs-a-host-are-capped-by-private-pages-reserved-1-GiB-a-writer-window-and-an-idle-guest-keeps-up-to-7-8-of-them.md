---
id: TASK-443.3
title: >-
  Investigate: VMs a host are capped by private pages reserved (1 GiB a writer
  window), and an idle guest keeps up to 7/8 of them
status: To Do
assignee: []
created_date: '2026-11-15 12:24'
labels:
  - perf
  - review-2026-11-15
dependencies: []
documentation:
  - docs/reviews/2026-11-15-sandbox-performance.md
parent_task_id: TASK-443
priority: high
ordinal: 837000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
PRIVATE_MAX_DEFAULT is 1 GiB (crates/virtio_extent/src/config.rs); CopyOnWrite::reserve (crates/fxdax/src/dax.rs) pins it whole at WINDOW, and Arenas::pin (crates/physmem/src/pmm/arenas.rs) refuses past 7/8 of the allowance or the tenant's cap: ~105 VMs on a 128 GiB host at best, 7 for a tenant limited to 8 GiB, then WINDOW fails ENOMEM. Separately guest/linux/fs/fxfs/dax.c fxfs_dax_work relieves clean private pages only near the bound, so a sandbox that overwrote a few hundred MiB then idles keeps them in host memory. The perf gate's density phase runs 3 VMs and reaches neither. Found by reading the code, not measured (docs/reviews/2026-11-15-sandbox-performance.md).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Idle VMs with the default config are started on one host until WINDOW fails, logging pinned bytes and the allowance; and fxdax_extentd_vm_window_private_pages is read after a write burst and five idle minutes
- [ ] #2 The notes give the measured cost and the workload size at which a customer would feel it; a fix, if needed, is filed as its own task
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
