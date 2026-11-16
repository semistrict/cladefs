---
id: TASK-483
title: >-
  The simulated extent device's window checks wrap: guest/sim/kernel/device.c
  answers out-of-range MAP, UNMAP, PRIVATE and READ as the device would not
status: To Do
assignee: []
created_date: '2026-11-16 06:19'
labels:
  - bug
  - testing
dependencies: []
priority: medium
ordinal: 881000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-470 (looking around its blind spot: a fake that differs from the real device). guest/sim/kernel/device.c, the extent device under the guest's Fxfs in the simulation, checks a window's range as `window_offset + len > device.window_len` (map_zeros, map_one, do_private), which wraps in u64: a range near u64::MAX passes and indexes device.pages out of bounds; unmap_one checks nothing (range, alignment); virtio_extent_submit's READ checks neither offset, length nor alignment before host_store_read(extent->store_offset + req->offset, ...). The real device (crates/virtio_extent, protocol::Bounds since TASK-470) refuses every such request with EINVAL, as doc-4 says, so a guest bug sending one passes the simulation silently or corrupts it, where the real device would fail the request.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The simulated device refuses with EINVAL every MAP, MAP_ZEROS, UNMAP, PRIVATE and READ doc-4 calls out of range (unaligned, past the window or max_extent, or wrapping), as crates/virtio_extent's Bounds does
- [ ] #2 A simulation test sends each kind at a wrapping range and asserts EINVAL; it fails with the checks taken out
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
