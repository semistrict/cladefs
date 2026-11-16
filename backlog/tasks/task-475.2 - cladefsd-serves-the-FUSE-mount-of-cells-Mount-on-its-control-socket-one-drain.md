---
id: TASK-475.2
title: >-
  cladefsd serves the FUSE mount of cells: Mount on its control socket, one
  drain
status: Done
assignee:
  - '@claude'
created_date: '2026-11-16 02:25'
updated_date: '2026-11-16 05:36'
labels:
  - host
  - fuse
dependencies:
  - TASK-475.1
parent_task_id: TASK-475
priority: high
ordinal: 879000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Second step of TASK-475. `cladefs-mount --control SOCKET --stores DIR MOUNTPOINT` is a process of its own, holding the cells it attaches as the daemon holds VMs' cells, with its own bucket client, part cache and upload budget. cladefsd takes it in: with a mount point configured, it mounts the directory of cells and serves the Mount service (proto/cladefs/mount/v1) on its control socket beside Extentd and health. `cladefs-mount STORE MOUNTPOINT`, one store mounted alone (pjdfstest, images), stays a tool.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 cladefs-mount --control and its attach, create-volume, detach and list commands are gone; cladefsctl calls Mount on the daemon's socket
- [x] #2 SIGTERM drains both: the mount unmounted lazily and its cells written back and detached, within the same bound as the VM stores, shown by a test
- [x] #3 The FUSE tests that ran cladefs-mount --control run cladefsd, on Linux
- [x] #4 With CLADEFSD_MOUNT naming a mount point (and CLADEFSD_MOUNT_STORES a directory), cladefsd mounts it and serves Mount on its control socket; without, it mounts nothing, and Mount calls fail UNIMPLEMENTED (gRPC's answer for a service not served)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. extentd/mount.rs: Config::from_env (CLADEFSD_MOUNT, _MOUNT_STORES, _MOUNT_ALLOW_OTHER), start() mounts the stores mount before the control API serves, service() gives the Mount service, close() unmounts lazily and joins.
2. fuse::control::service replaces its own server; serve_control adds it (Drained, so counted and refused while draining); absent without a mount -> UNIMPLEMENTED.
3. drain closes the mount after calls end, within the bound; Left gains mount.
4. cladefs-mount keeps single-store mode only; cladefsctl mount attach|create-volume|detach|list.
5. Tests: fuse_mount.rs drives cladefsd + cladefsctl mount; SIGTERM drains VM store and mount (later placement reads the file from the bucket); no-mount daemon answers UNIMPLEMENTED. Docs: cladefsd header, README, integrator guide, mount.proto (Go regenerated).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Linux (ubu1, fxjob, Rust 1.100): fuse_mount incl. one_mount_attaches_and_detaches_many_stores (now through cladefsd + cladefsctl mount), sigterm_drains_vms_stores_and_the_mount_of_cells, a_daemon_mounting_no_cells_serves_no_mount_calls pass. Bug found by the drain test and fixed: the control service held the mount's Server strongly, so stores closed by the drain's join (Server::unmount keeps slots) were never dropped and their uploaders ran past the drain (exit 1 after 60 s); Control now holds a Weak<Server>, calls after close answer UNAVAILABLE.

DoD #1: scripts/test.sh clippy+workspace on commit 1270ab9fe: macOS 2517/2517, Linux (ubu1, fxjob) 2827/2827; CI's clippy rules clean on both.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
cladefsd mounts tenants' cells with FUSE (CLADEFSD_MOUNT, CLADEFSD_MOUNT_STORES) and serves Mount on its control socket; drain closes the mount within its bound; cladefs-mount --control replaced by cladefsctl mount. Drain test found and fixed stores kept open past the drain (control holds the server weakly). Verified: fuse_mount tests on Linux, full suites.
<!-- SECTION:FINAL_SUMMARY:END -->
