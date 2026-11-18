---
id: TASK-586
title: A failed worker start leaves the extent device activated with no workers
status: To Do
assignee: []
created_date: '2026-11-18 15:29'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 977000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), firecracker review; verified against the code by independent reviewers. Severity: medium.

**Where:** `vendor/firecracker/src/vmm/src/devices/virtio/extent/device.rs`: `rg -n -F 'if let Err(err) = self.resume_worker() {' vendor/firecracker/src/vmm/src/devices/virtio/extent/device.rs`

Extent::kick only logs when resume_worker fails, then carries on. Kick is what restarts the workers that take_snapshot/pause_worker stopped. The VM then runs with every extent request left on its ring for ever, against AGENTS.md's fail-hard rule. In activate_confined, the device state is set to Activated before resume_worker runs (device.rs:238). If resume_worker then fails, the transport marks DEVICE_NEEDS_RESET and leaves device_activated false, but Extent::is_activated() stays true. A later reset is never forwarded to the device, because the transport resets only when device_activated is true. The next DRIVER_OK calls activate, and its assert!(!self.is_activated()) (device.rs:221) panics on the fc_extent_activate thread. With panic=abort the VMM dies of SIGABRT. Separately, reset() turns a failed cladefs::thread::run into None (device.rs:394). The PCI transport then keeps device_activated with status INIT, blocking re-initialization while the old workers keep serving the rings the driver reset.

**Trigger.** Pause the VM, PUT /snapshot/create (take_snapshot -> pause stops the workers), then PATCH /vm Resumed -> resume_vm -> kick_virtio_devices -> Extent::kick -> resume_worker. start_workers fails: the spawner's thread spawn gets EAGAIN at the jail's pids.max or RLIMIT_NPROC, or duplicating a queue eventfd hits EMFILE. An error is logged, the vCPUs resume, the guest's Fxfs commits and reads queue up and are never taken, and the guest hangs while the process looks healthy. Activation variant: the same failure at DRIVER_OK -> NEEDS_RESET; the driver unbinds (the status-0 write is not forwarded) and binds again -> activate -> assert -> abort.

**Why the tests stay green.** TASK-79.44's workers_start_all_or_none tests the crate's Workers::start. Nothing injects a start failure into the Firecracker adapter's kick or activate paths: Worker::spawn and the spawner have no seam, and the guest cases never run short of threads or fds.

**Backlog.** Related: TASK-79.44 (Done) made Workers::start all-or-none but did not touch the adapter's swallowed kick error or its activation state order. TASK-535 (To Do) is an epoll init failure inside a worker thread. TASK-79.8 and TASK-141.3 (Done) cover NEEDS_RESET and reopening.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 If restarting the extent workers fails when a VM resumes after a VM snapshot (Extent::kick), the VM is stopped or the resume fails. The guest never runs with its extent requests left on the ring unserved.
- [ ] #2 After a failed activation, the extent device is not activated, and the driver's next DRIVER_OK activates it again without the VM process aborting. A reset whose worker start fails leaves the device in a state the PCI transport can initialize again, with no old workers serving the rings the driver reset.
- [ ] #3 An adapter test with a cladefs::thread spawner that refuses the Nth spawn covers a failed activate followed by a second activation, and a kick after take_snapshot. It goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
