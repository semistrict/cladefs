---
id: TASK-662
title: Cell snapshot region lengths can wrap the guest-memory reservation
status: To Do
assignee: []
created_date: '2026-11-19 17:27'
labels:
  - security
dependencies: []
priority: medium
type: bug
ordinal: 1046000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the 2026-11-19 kernel sandbox security review. **Where:** `vendor/firecracker/src/vmm/src/cladefs_memory.rs:115-136`.

Cell restore sums region lengths with unchecked `sum`, then advances with unchecked `offset += len` and `reservation.base.as_ptr().add(offset)`. Two lengths such as `usize::MAX - 4095` and 8192 wrap the reservation to 4096 bytes; later pointer arithmetic and raw MmapRegion construction no longer refer to that reservation. `with_raw_mmap_pointer` requires the caller to guarantee validity. This is a restore-time memory-safety defect when snapshot state is not fully trusted; no subsequent guest escape has been demonstrated.

**Blind spot:** snapshot sanity checks cover existence and plugged slots, not arithmetic overflow or region fit. **Structure:** compute the reservation with checked addition and validate each region and final offset before any raw pointer operation, rejecting malformed state before daemon or KVM work.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Reject overflowing regions before pointer arithmetic or allocation, with an ASan/UBSan regression
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
- [ ] #4 Firecracker tests pass on ubu1 and scripts/upstream-changes.py --check passes
<!-- DOD:END -->
