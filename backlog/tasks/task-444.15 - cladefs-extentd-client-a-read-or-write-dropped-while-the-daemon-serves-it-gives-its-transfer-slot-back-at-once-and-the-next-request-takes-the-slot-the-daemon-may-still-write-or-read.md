---
id: TASK-444.15
title: >-
  cladefs extentd client: a read or write dropped while the daemon serves it
  gives its transfer slot back at once, and the next request takes the slot the
  daemon may still write or read
status: Done
assignee: []
created_date: '2026-11-16 14:48'
updated_date: '2026-11-18 03:37'
labels:
  - bug
  - unsafe
dependencies: []
parent_task_id: TASK-444
priority: high
ordinal: 882000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Slot::drop pushed the slot's index back to the free list as read_async or write_with_barriers_async was dropped, whether or not the daemon had answered. Until it answers, the daemon may write the slot (a READ into it) or read it (a WRITE from it): the next request on the slot read the dropped read's bytes, or the daemon wrote another request's. Latent while virtio requests await to completion; dropping a future is safe. Found by TASK-444.4's audit (both Codex models Incorrect on client.rs's Transfer). Test: extentd::tests::a_dropped_read_keeps_its_transfer_slot_until_answered (red: 8 slots free, 7 expected).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A slot is free again only once the daemon answered its request and its caller is done with it
- [x] #2 a_dropped_read_keeps_its_transfer_slot_until_answered passes
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
A slot is an Arc<TakenSlot> held by its caller and by its request's waiting entry (Client::request_keeping), given back once both let go. Verified: cladefsd::tests::a_dropped_read_keeps_its_transfer_slot_until_answered red before (8 free, 7 expected), green after; 130 cladefs heap/transfer/lifecycle/cladefsd tests pass on ubu1. Pushed c97c3490d and cf0b3e934 (failpoint name from the server's table), main 558c354e2.
<!-- SECTION:FINAL_SUMMARY:END -->
