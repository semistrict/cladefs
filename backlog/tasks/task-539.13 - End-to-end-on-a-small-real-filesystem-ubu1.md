---
id: TASK-539.13
title: End to end on a small real filesystem (ubu1)
status: Done
assignee: []
created_date: '2026-11-18 14:17'
updated_date: '2026-11-18 17:42'
labels:
  - extentd
  - disk
dependencies:
  - TASK-539.9
  - TASK-539.12
documentation:
  - backlog/docs/doc-22 - Disk-governor-the-hosts-disk-as-a-managed-resource.md
parent_task_id: TASK-539
priority: medium
type: feature
ordinal: 945000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The guarantee must be shown on a real filesystem with a neighbour and a stalled bucket, not only reasoned from the parts. (doc-22 task T14.)
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The Linux integration test under crates/cladefs/tests is ignored when unprivileged and run as root through fxjob (the TASK-307 pattern). The daemon's part cache and stores sit on a 1–2 GiB RAM-backed loop ext4 with CLADEFSD_DISK_HEADROOM=256 MiB.
- [x] #2 With the cache filled by reads, the bucket gated and a neighbour fallocating, the run checks all of these: a 10 ms statvfs sampler never sees f_bavail below a band; the cache empties before the first refused write; guests see only ENOSPC on WRITE; no store fails; commits go on.
- [x] #3 After the bucket is ungated, uploads drain and writes are admitted again. The cache evicts within two INTERVALs of the neighbour, and cladefsd_host_disk_other_bytes stays under 64 MiB.
- [x] #4 It runs in under 5 minutes, one fxjob job at a time.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Found on the way: a guest's commit(None) waits for its upload, so a test of an unreachable bucket commits ordered, as guests' fsyncs do. Uncommitted data costs 7x against limits (the reserve): the base had to commit as it wrote.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
crates/cladefs/tests/disk_full.rs, root's (ignored otherwise; run on ubu1 under fxjob with sudo): a 1.5 GiB ext4 over a loop device on tmpfs holds the daemon's stores and part cache, 256 MiB headroom, its bucket elsewhere and gated with CLADEFS_BUCKET_FAULTS. Checks: on a disk of its own, other_bytes under 64 MiB; a neighbour's 200 MiB fallocate given back by the cache within two governor looks; the guest refused with ENOSPC alone, after 100+ MiB, the cache at most a band of the headroom then; its store committing (ordered) on, writing again after a commit frees its open extents' reserve and refused again; the bucket back, writes admitted again; a 10 ms statvfs sampler never under a quarter of the headroom free. Passes in 61 s.
<!-- SECTION:FINAL_SUMMARY:END -->
