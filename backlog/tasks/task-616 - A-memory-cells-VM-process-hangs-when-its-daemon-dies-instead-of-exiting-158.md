---
id: TASK-616
title: A memory cell's VM process hangs when its daemon dies instead of exiting 158
status: To Do
assignee: []
created_date: '2026-11-18 15:34'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 1007000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), firecracker review; verified against the code by independent reviewers. Severity: low.

**Where:** `vendor/firecracker/src/vmm/src/cladefs_memory.rs`: `rg -n -F 'let memory_cell = MemoryCell::open_forked(&config.cladefsd_socket, store, from, size as u64)' vendor/firecracker/src/vmm/src/cladefs_memory.rs`

The second stage relies on a contract: a window fault that fails after its connection is lost (window.lost(), which is client.lost.happened()) waits forever with std::future::pending (uffd.rs, TASK-231). This assumes the VM process is already exiting with CladefsdLost because its connection's on_lost listener exits. The extent device registers that listener before it opens (virtio_extent device.rs connect, TASK-50.2/232). MemoryCell::open_forked connects with a bare DaemonConnection::connect, and cladefs_memory::guest_memory registers nothing either. When the daemon dies, a guest access to a memory node not yet mapped goes Reach -> Mapper -> FileSet read through DaemonDevice -> EIO. lost() is true, so the fault future pends forever: the vCPU blocks in the userfaultfd wait and the fault thread is stuck for good. Nothing exits. If an extent device uses the same daemon, its own listener exits 158 and hides the problem. A VM restored with Cell memory and no cladefs disk, or with devices on another socket, hangs for ever. This is the same class as TASK-79.42 (Done), a connection opened without its on_lost.

**Trigger.** Restore a snapshot with mem_backend Cell and no cladefs device (virtio-block root, or none). Kill or restart cladefsd. The guest touches memory in a tree node it has not touched before. The vCPU never returns from KVM_RUN, the process stays alive and never exits 158, and the VM manager sees a live, hung VM.

**Why the tests stay green.** The daemon-lost cases (fxfs-guest-daemon-lost, TASK-50.2/231/232) all run VMs with extent devices, whose own listener exits the process. No case restores Cell memory without an extent device and then kills the daemon. The uffd unit test faults_failed_as_their_daemon_goes_wait checks that the fault pends, which is right only if someone exits.

**Backlog.** RELATED: same class as TASK-79.42 (Done, a follower's reopen without on_lost) on another connection; TASK-231, TASK-232 and TASK-50.2 (Done) cover VMs with extent devices only; TASK-456.3 (In Progress) has no daemon-loss criterion.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A VM process restored from a Cell memory backend with no extent device exits 158 within a bound after its daemon dies, whether the guest then touches a mapped or an unmapped memory node.
- [ ] #2 A regression test (a VMM test, or a unit test of guest_memory against a fake daemon that closes its socket) asserts the on_lost exit runs, and goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
