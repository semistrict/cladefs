---
id: TASK-640
title: 'FakeDevice::close seals nothing, unlike every real device''s close'
status: To Do
assignee: []
created_date: '2026-11-18 17:49'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 1027000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found while adding close-and-reopen tests for TASK-540. FakeDevice::close (crates/storage_device/src/fake_device.rs, `self.closed.store(true, Ordering::Relaxed);`) only sets a flag. The real devices commit on close: DaemonDevice::close commits at the uploaded level, and ImageDevice::close flushes. So on FakeDevice, whatever a file set frees in close() is never sealed by a barrier, and recorded crash images (`Crashes`) never include a crash after close. Any test that closes and then crashes gets a different device than production does, which hides close-ordering bugs of TASK-540's kind.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 FakeDevice::close seals as the real devices' close does: a barrier, recorded by Crashes, after which the device refuses requests.
- [ ] #2 A test closes a file set after a commit that frees extents and checks that the crash image recorded at close holds the frees.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
