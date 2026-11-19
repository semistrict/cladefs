---
id: TASK-666
title: Virtio request handlers accept unknown flags
status: To Do
assignee: []
created_date: '2026-11-19 17:28'
labels:
  - security
dependencies: []
priority: low
type: bug
ordinal: 1049000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the 2026-11-19 kernel sandbox security review. **Where:** `crates/virtio_extent/src/requests.rs:110-349`; doc-4 requires EINVAL for unknown flags.

READ, ROOT, INFO, COMMIT, UNMAP, FREEZE, RESIZED, ADVANCE, LET_GO, PIN and UNPIN do not consistently reject unknown bits, unlike WRITE, FREE, MAP and PRIVATE. This is a protocol-validation defect and version-negotiation hardening gap, not a demonstrated host escape.

**Probe, confirmed on ubu1:** the real guest-kernel `extent-guest-attack` sent high-bit flags through the real driver/Firecracker/daemon path. INFO, ROOT, READ, COMMIT, UNMAP and FREEZE all returned an unexpected status rather than EINVAL (`unexpected request statuses: mask 0x3f`). The other nine cases—misaligned, overflowing and out-of-window ranges and cross-cell READ/MAP—were refused correctly. The ordinary `extent-guest` control passed. **Structure:** one per-type flag mask should be validated centrally before any handler side effect.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Reject every unknown flag with EINVAL before side effects on every request type
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
- [ ] #4 cargo nextest run --workspace and affected guest cases pass on ubu1
<!-- DOD:END -->
