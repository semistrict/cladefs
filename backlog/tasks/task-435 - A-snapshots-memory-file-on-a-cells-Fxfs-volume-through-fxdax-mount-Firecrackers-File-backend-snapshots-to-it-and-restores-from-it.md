---
id: TASK-435
title: >-
  A snapshot's memory file on a cell's Fxfs volume through fxdax-mount:
  Firecracker's File backend snapshots to it and restores from it
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 11:12'
updated_date: '2026-11-15 12:59'
labels:
  - snapshot
  - fuse
dependencies: []
priority: medium
ordinal: 783000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The user (2026-11-15) is exploring keeping a VM snapshot's memory file, today on a GCS FUSE mount, as a volume in the same cell as the VM's disk, so a cell is everything for one VM. Before choosing between serving it through fxdax-mount (Firecracker's File backend, MAP_PRIVATE of the file) or a UFFD handler, the FUSE path must be shown to work: no test mmaps a file of the mount, nor has Firecracker written or restored guest RAM through it.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A guest test snapshots a VM (extent device, DAX window) with mem_file_path a file of a volume of a cell attached through fxdax-mount --control, and restores it twice (zygote) with the File backend from that file, each restored guest checking what it wrote before the snapshot
- [ ] #2 scripts/test-e2e.sh builds fxdax-mount and hands it to the cases (FXDAX_MOUNT_BIN), prebuilt artifacts included
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-15, ubu1 (branch t435-memory-mounted, not on main): fxfs-guest-snapshot-memory-mounted-dax and -uffd-dax pass with FXDAX_DAX_WINDOW_MIB=2048 (results3/results4 under /mnt/fxdax-scratch/t435). 512 MiB VM: snapshot 678 ms to a local file, 789 ms through fxdax-mount (4.8 s on a loaded host); restores 36-37 ms both; UFFD example handler 368-384 ms (it reads the whole file through the mount first). The cases' extra ~30 s is the test's own reopening of the memory volume with Storage::check (full fsck): 25.5 s; detach 0.7 s. Without the window cap the restores' windows were refused ENOMEM on a loaded host (TASK-442). The user then chose a raw memory volume kind served inside Firecracker as a copy-on-write window (forks share pages), so this FUSE path is kept as evidence, not merged.
<!-- SECTION:NOTES:END -->
