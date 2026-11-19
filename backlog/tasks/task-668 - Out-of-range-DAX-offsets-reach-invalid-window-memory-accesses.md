---
id: TASK-668
title: Out-of-range DAX offsets reach invalid window memory accesses
status: To Do
assignee: []
created_date: '2026-11-19 17:29'
labels:
  - security
dependencies: []
priority: low
type: bug
ordinal: 1051000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the 2026-11-19 kernel sandbox security review. **Where:** `guest/linux/drivers/virtio_extent/driver.c:1013-1064`.

`window->len / PAGE_SIZE - pgoff` underflows when pgoff is past the window, allowing vext_window_make to index invalid parts. `vext_dax_zero_page_range` also memsets after a zero direct-access result (uninitialized kaddr) and zeroes all requested pages after a short result. Normal allocators constrain pgoff; no unprivileged route or host escape is demonstrated, so this is guest memory-safety hardening.

**Structure:** reject pgoff/length overflow and out-of-window ranges before subtraction, return errors for zero/short direct access, and zero only a validated returned range.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Reject out-of-range DAX accesses before subtraction or memory access and add KASAN probes
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
- [ ] #4 Guest kernel and KASAN guest cases pass on ubu1
<!-- DOD:END -->
