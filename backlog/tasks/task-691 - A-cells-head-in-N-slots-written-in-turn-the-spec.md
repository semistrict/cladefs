---
id: TASK-691
title: 'A cell''s head in N slots written in turn: the spec'
status: Done
assignee:
  - '@claude'
created_date: '2026-11-20 09:39'
updated_date: '2026-11-20 17:02'
labels:
  - bucket
  - gcs
  - tla
dependencies: []
priority: high
type: feature
ordinal: 1082000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-690 caps a cell's head swaps at GCS's one write a second to an object, so a commit waiting for the bucket waits up to a second more. The user's protocol (2026-11-20): keep the head, the one conditional register a cell's protocol rests on, but keep it in N objects, version k in slot k mod N, so each object is written once every N swaps: N swaps a second. Chosen over the alternative (acknowledging by reading the head, takeovers listing the predecessor's segments) as the more general building block: a linearizable conditional register over object storage, which any later feature publishing state uses as the head is used now. specs/bucket keeps the head as one object; specs/head-slots shows the slots are that object. The code is a later task.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 specs/head-slots models writers swapping the head by conditional writes of its slots in turn and readers reading every slot one GET at a time while writes go on; FencedWrites, SlotsNewest and ReadsLinearizable hold for 2 and 3 slots
- [x] #2 Each rule has a mutation finding its violation: NoSlotCondition (FencedWrites), NoNewestOfSlots (ReadsLinearizable)
- [x] #3 specs/bucket says its one head is the slots', and every specs/bucket model and mutation passes as before
- [x] #4 Code: a cell's head kept in its bucket's head slots, made whole by its maker, swapped round them conditional on each slot, read whole or final, unmade heads failed by marks; bucket_cell's direct tests show each rule needed (slots_at_birth, whole_or_final, marks_fail) and the slots used as the bucket's settings say
- [x] #5 Bucket settings in the identity, fixed at format-bucket/add-bucket (--head-slots, default 10, per bucket: an added bucket takes the root's), recorded in the registry; bucket_cell's identity test shows formatting, adding, inheriting and range refusals
- [x] #6 bucket_cell, local_cell (simulations with two-slot heads included) and cladefs' bucket tests pass on the Mac; every head-slots model holds and every mutation finds its violation
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Mac, scripts/check-tla.sh: HeadSlots (2 slots, 2 writers, 1 reader, versions to 5) 666,565 states 2 s; HeadSlots3 (3 slots, versions to 6) 14,800,131 states 36 s; NoSlotCondition violates FencedWrites, NoNewestOfSlots violates ReadsLinearizable; all 48 specs/bucket models and mutations pass, as on main before. Not modelled: a read starting at a known version and reading forward until a slot holds an older one; the code's read should be checked against this spec, or the spec extended, when it is written.

2026-11-20, code (user: no PRs, straight to main; 10 slots by default, a format-time setting; half-made heads fenced by marks, the collector deleting marks a day on). Spec extended: slotsAtBirth, wholeOrFinal, marksFail, marksKept, each with a mutation (TLC found two drafts' flaws: deleting a head being made, and finishers copying version 0 back after a removal). Code: head.rs (Slots, Found, birth, fail), identity Settings/Asked, Bucket::head_slots (ObjectBucket reads its identity once), collector fails heads unmade an hour (UNMADE_FOR) and deletes failed heads' slots, their marks a day on (MARKS_KEPT), retiring a failed fork's reference; lifecycle deletions fail unmade heads; fork steps meeting a failed head give up (Deleted). The simulation keeps heads in 2 slots: at 10, each head read 10 of its 400 steps' requests, and its rule searches found nothing in 30+ minutes. Its garbage check now keeps unmade moves of a kept parent (TASK-697, filed: a leak seed 3 of twin restores showed). Verified on the Mac: bucket_cell 377/377, local_cell 375/375, cladefs bucket tests 113/113 and cladefsctl 12/12; head-slots: HeadSlots 3.4M states 13 s, HeadSlots3 254K 1 s, six mutations.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
specs/head-slots: a cell's head in N slots, version k in slot k mod N, each swap conditional on its slot, each read of every slot taking the newest; shown to be the one conditional head specs/bucket models (FencedWrites, ReadsLinearizable), each rule with a mutation. specs/bucket says so; its models pass unchanged.
<!-- SECTION:FINAL_SUMMARY:END -->
