---
id: TASK-575
title: A VM snapshot that fails after a device made its version leaks that version
status: To Do
assignee: []
created_date: '2026-11-18 15:28'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 966000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), cross-host-take-restore review; verified against the code by independent reviewers. Severity: medium.

**Where:** `vendor/firecracker/src/vmm/src/persist.rs`: `rg -n -F 'vmm.take_extent_snapshots()' vendor/firecracker/src/vmm/src/persist.rs`

create_snapshot makes a version of every extent device's cell first (take_extent_snapshots -> Extent::take_snapshot -> virtio_extent Device::take_snapshot -> store.make_version()), keeping each ID only in the device's in-memory `snapshot` field. Then come save_state, snapshot_state_to_file and snapshot_memory_to_file. Any failure after a device's make_version returns an error and drops nothing: a later device's NotFrozen/EBUSY/Version error in the loop, window_mappings() failing after make_version, a save_state error, or ENOSPC writing the snapshot file. The version IDs are lost: no file names them, and the next attempt overwrites `snapshot`. The cell's head keeps naming each such version, so the collector keeps every extent the guest frees afterwards, for good, unless someone lists the cell's versions and drops IDs they cannot identify. No doc tells the VM manager to do that. Each retried failed snapshot leaks another version per device that succeeded. Since MAX_VERSIONS is not enforced, nothing bounds the accumulation; once it is enforced, these leaks will instead make later snapshots fail with EMLINK.

**Trigger.** A VM with two extent devices (root filesystem and data cell). The VM manager freezes only the first filesystem, or the second's window still holds private pages. CreateSnapshot: device 1 makes version V1, device 2 fails NotFrozen/EBUSY, create_snapshot returns an error. The manager fixes the freeze and retries, making V2, and so on: V1 stays named by cell 1's head and pins its state. The same happens if the snapshot file's directory is full (snapshot_state_to_file ENOSPC) after all devices made their versions.

**Why the tests stay green.** Snapshot tests exercise successful snapshots and single refusals (NotFrozen, private pages via TASK-203) on one device. None checks a cell's versions after a failed multi-device snapshot or a failed snapshot-file write.

**Backlog.** New. Same class as TASK-514 (To Do), a fork's version outliving a host dying mid-fork, on another path; TASK-203 (Done) covers single-device refusals only.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 When create_snapshot fails after some extent devices made their versions (a later device NotFrozen, EBUSY or ReadOnly, window_mappings failing, save_state failing, or the snapshot file write failing with ENOSPC or an unwritable path), every version it made is dropped: ListVersions on each device's cell shows none of them.
- [ ] #2 Retrying a failed VM snapshot N times leaves each cell's version count unchanged; the retry that succeeds adds exactly one version per cell.
- [ ] #3 A Firecracker/virtio_extent test with two devices, the second not frozen, asserts create_snapshot fails and ListVersions on the first device's store is empty, plus a variant failing snapshot_state_to_file through an unwritable path; both go red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
