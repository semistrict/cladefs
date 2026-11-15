---
id: TASK-411
title: >-
  Per-tenant encryption at rest: tenant KEKs in a KMS, cell KEKs, volume DEKs
  (doc-16)
status: To Do
assignee: []
created_date: '2026-11-14 18:33'
updated_date: '2026-11-15 03:24'
labels: []
dependencies: []
documentation:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
priority: medium
type: feature
ordinal: 797000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Tenants need BYOK (their root key in a KMS they control and can revoke), crypto-shredding (destroying that key leaves their data unreadable in the bucket, its versions and hosts' disks), and a bucket that tells a reader nothing of their data. Today every object is plaintext. doc-16 is the design and records the user's decisions of 2026-11-14; host compromise is out of scope, as DAX needs plaintext pages in the extent cache.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A tenant whose KEK is destroyed in the KMS has no readable data in the bucket, in WAL segments, the part cache or open segments: a test reads each and fails to decrypt
- [x] #2 A bucket read without keys shows no byte of a volume's file data: a test writes known content and finds it in no object or local file
- [ ] #3 Forks, points, shared points, followers, FUSE mounts, sqlite volumes, restores and the collector work on encrypted stores: their existing tests pass with encryption on
- [ ] #4 doc-16, doc-7 and doc-14 describe what main does
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
TASK-411.3: sqlite volumes, FUSE mounts, extentd and restores have no encrypted test variants yet (Linux-only daemon paths; restores fork across cells, TASK-411.4); AC #3 waits on them.

TASK-411.7: AC #3 encrypted variants now written. macOS (run): sqlite volumes, sqlite_extents tests/image.rs a_sealed_database_on_an_image_survives_closing (plain twin asserts the data is in a file in the clear; sealed, in none); FUSE mounts' server, fxdax tests/fuse.rs a_sealed_store_served_reads_back_and_holds_nothing_in_the_clear. Linux-only (compiled with cargo clippy --target x86_64-unknown-linux-gnu, not run): sqlite_extents tests/daemon.rs a_sealed_database_on_the_daemon_reads_in_place_and_survives_reopening (FXDAX_KMS); fxdax tests/fuse_mount.rs a_sealed_store_mounted_holds_nothing_in_the_clear (kernel FUSE); fxdax tests/control.rs sealed_stores_served_and_shredded_through_the_control_api (extentd serving VMs' stores of sealed cells through its socket, then ShredCell/ForgetTenantKeys). Still without a variant: the VMM and guest e2e paths (scripts/test-e2e*.sh).

2026-11-15: subtasks .1-.10 (but .11), .13, .14 done at 594ff82e9, verified on macOS and Linux. AC #1: TASK-411.7's shredded_tenants_leave_none_of_their_volumes_readable_on_any_host (no plaintext in open segment, log, part cache; fresh opens fail naming the tenant). AC #2: TASK-411.3's cells_keep_keys_wrapped_and_one_unwrap_takes_a_cell finds no key or data bytes in any object; TASK-411.5/.9 tests find none in local files. AC #3 open: encrypted variants exist for forks, points, shared points, followers, restores, the collector, FUSE (server and kernel mount), sqlite (image and daemon) and extentd through the control API, all passing on Linux; the VMM and guest e2e paths (scripts/test-e2e.sh) have none yet. AC #4 open until doc-14 is checked against what TASK-411.5/.9 made.
<!-- SECTION:NOTES:END -->
