---
id: TASK-442
title: >-
  Guest cases' large DAX windows reserve private pages off the host's MemTotal,
  the daemon's memory off what is free: restores fail ENOMEM on a loaded host
status: To Do
assignee: []
created_date: 2026-11-15 11:57
updated_date: '2026-11-16T21:40:00Z'
labels:
  - tests
  - dax
dependencies: []
priority: medium
ordinal: 786000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Window::Large sizes a case's DAX window at the host's MemTotal (guest/tests/test_cases/src/common.rs window_bytes, 27 GiB on ubu1), and a copying window reserves a quarter of its pages as private pages against its tenant's arena at creation (dax.rs CopyOnWrite::reserve, physmem::pin, ENOMEM past what nothing can evict). The test daemon's memory is a page cache of what of the host's is free (TASK-358). On 2026-11-15 on ubu1, with another job holding 12 GiB, fxfs-guest-snapshot-memory-mounted-dax and -uffd-dax (TASK-435) failed: one restore's window was made, the next refused, 'extentd: creating a window: ENOMEM', Firecracker 'Cannot activate the restored device: Extent device: creating the DAX window: ENOMEM'. The same cases passed an hour earlier on a less loaded host. Every case with a large window and two or more VMs at once is exposed (fxfs-guest-snapshot-dax, migrate-tenant).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A case's VMs' windows are made whatever the host's other load, as long as the daemon has memory for what they hold: their private reservations sized to what the case's guests can write, not to the host's MemTotal, shown by running fxfs-guest-snapshot-dax with the daemon's memory capped below 3 x MemTotal/4
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-15 again on ubu1 (branch t456-3, fxjob --mem 8G, CLADEFS_DAX_WINDOW_MIB=2048): fxfs-guest-snapshot-dax's second restore's window ENOMEM, and fxfs-guest-snapshot-memory-volume-dax's page-ins 'Errno(ENOMEM): no pages to read into': the daemon's memory, a page cache of what of the host's and the run's cgroup's memory is free (test-e2e.sh, TASK-358), left little after the run's own build in the same cgroup; the same cases passed earlier in the day. CLADEFS_EXTENTD_MEMORY set explicitly sidesteps it.

Superseded by TASK-501: copying windows reserve nothing against the daemon memory, so a loaded host no longer refuses a restore window ENOMEM for its private pages.
<!-- SECTION:NOTES:END -->
