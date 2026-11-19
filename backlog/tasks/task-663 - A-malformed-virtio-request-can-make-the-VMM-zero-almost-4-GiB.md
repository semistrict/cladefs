---
id: TASK-663
title: A malformed virtio request can make the VMM zero almost 4 GiB
status: To Do
assignee: []
created_date: '2026-11-19 17:27'
labels:
  - security
dependencies: []
priority: medium
type: bug
ordinal: 1047000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the 2026-11-19 kernel sandbox security review. **Where:** `crates/virtio_extent/src/requests.rs:81-96`; `crates/virtio_extent/src/buffers.rs:79-85`.

Writable chains are capped only at u32::MAX. On any error, handle zeroes every writable byte not returned. An unknown request with 255 aliased 16 MiB writable descriptors therefore zeroes 0xff000000 bytes before returning EINVAL. Aliasing avoids proportional guest RAM; distinct descriptors also consume host memory bandwidth and populate guest RAM. The review probe wrote exactly 0xff000000 bytes (16 ms on macOS and 88.6 ms on ubu1 in its cache-hot alias case).

**Blind spot:** tests cover sums above u32::MAX but not near-limit error-path work. **Structure:** bound total descriptor work to a small protocol maximum and do not use a u32 wire-length field as a work budget.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Reject near-limit malformed requests without large writes and cover aliased descriptors
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
- [ ] #4 cargo nextest run --workspace passes on macOS and Linux (ubu1)
<!-- DOD:END -->
