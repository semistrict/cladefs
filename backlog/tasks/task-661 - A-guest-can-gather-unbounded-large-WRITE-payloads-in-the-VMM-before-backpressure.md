---
id: TASK-661
title: >-
  A guest can gather unbounded large WRITE payloads in the VMM before
  backpressure
status: To Do
assignee: []
created_date: '2026-11-19 17:27'
labels:
  - security
dependencies: []
priority: high
type: bug
ordinal: 1045000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the 2026-11-19 kernel sandbox security review. **Where:** `crates/cladefs/src/cladefsd/client.rs:327-349,964-984`; `crates/virtio_extent/src/workers.rs:269-286`.

A worker pops all available chains. Each WRITE larger than the eight 256 KiB transfer slots is gathered into a fresh Vec before Client::call applies request-credit backpressure. One 256-entry queue of 8 MiB WRITEs can therefore hold about 1.94 GiB in the VMM before daemon/tenant budgets help; multiple queues multiply it. The guest can alias one data buffer, so guest RAM need not scale with VMM allocation.

**Probe, confirmed on ubu1:** `cladefsd::client::tests::large_writes_waiting_for_replies_do_not_gather_unbounded_payloads` issued 32 such writes against a consuming but non-answering daemon and measured **268,454,256 bytes** gathered in the VMM. The per-request linear behavior is therefore demonstrated; a full 256-entry queue extrapolates to about 1.94 GiB. **Blind spot:** queue and credit tests bound wire bytes after allocation, not VMM heap before it. **Structure:** reserve request/tenant credit before gathering, or gather directly into a bounded transfer mechanism; queue-pop and payload ownership must share one byte budget.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Bound VMM payload memory before gathering and verify a full queue with replies held
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
- [ ] #4 cargo nextest run --workspace passes on macOS and Linux (ubu1)
<!-- DOD:END -->
