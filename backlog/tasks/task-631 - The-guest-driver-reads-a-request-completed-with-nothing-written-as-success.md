---
id: TASK-631
title: The guest driver reads a request completed with nothing written as success
status: To Do
assignee: []
created_date: '2026-11-18 15:36'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 1022000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), virtio-host review; verified against the code by independent reviewers. Severity: low.

**Where:** `guest/linux/drivers/virtio_extent/driver.c`: `rg -n -F 'req->status = -(int)le32_to_cpu(req->resp.status);' guest/linux/drivers/virtio_extent/driver.c`

When the VMM adapter hands over Popped { buffers: Err } (Firecracker's ChainError: a descriptor that does not fit inside one guest memory region, or BuffersError::TooLong), Requests::serve logs the error and completes the request with used length 0 and no response. The crate's documentation says the driver is to tell this apart as "returned with nothing written". The guest driver cannot: vext_submit sets req->resp.status = 0 before queuing (guest/linux/drivers/virtio_extent/driver.c), and vext_reap ignores the used length and takes status from resp.status. So a WRITE, COMMIT or FREE the device never served reports success, and a READ reports success with whatever was already in the buffer. handle()'s 'no room for its response' path, which also returns 0, has the same result.

**Trigger.** A VMM adapter rejects a chain that is well formed in guest terms. Firecracker does this if a descriptor's range crosses two adjacent memory regions: its current x86_64 and aarch64 layouts leave gaps between DRAM regions, but libmachine or a future layout with adjacent regions or hotplugged memory would not. A misbehaving driver can also name more than 4 GiB of writable memory. Then: buffers() returns Err, Requests::serve calls complete(0), vext_reap computes status = -(0) = 0, and Fxfs sees its WRITE or COMMIT succeed although the daemon never received it. Data the guest thinks is written or committed is missing.

**Why the tests stay green.** workers_take_serve_and_return_requests and Firecracker's chains_writing_past_u32_are_refused only check the error and the used length 0. Nothing checks what the guest driver concludes from a length-0 completion, and no guest case produces a chain the adapter rejects.

**Backlog.** New. TASK-79.25 and TASK-417 (Done) made the device complete rejected chains with used length 0 and tested only the device side; nothing covers what the driver concludes.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A request the device completes with a used length shorter than its response (a rejected chain, or no room for a response) returns -EIO in the guest driver: WRITE, COMMIT and FREE never report success, and READ never returns stale buffer contents as success.
- [ ] #2 A test queues a chain whose data descriptor lies outside guest memory and asserts the driver-side result is -EIO (or the response holds a non-zero status); it goes red with the fix reverted.
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-19 security review: the first real-VM attack fixture accidentally placed its `vext_request` on the guest kernel stack, outside valid guest memory. Firecracker logged `a descriptor chain is not guest memory` for all 15 requests and completed each with used length 0. The guest initialized response status to zero, ignored the used length and returned success for every one (`unexpected request statuses: mask 0x7fff`). This directly reproduces the rejected-chain half of the bug through the real virtio/Firecracker path. The fixture was moved to heap memory and rerun to test its intended malformed fields separately.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
