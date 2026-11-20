---
id: TASK-691
title: 'A cell''s head in N slots written in turn: the spec'
status: Done
assignee:
  - '@claude'
created_date: '2026-11-20 09:39'
updated_date: '2026-11-20 09:53'
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
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
specs/head-slots: a cell's head in N slots, version k in slot k mod N, each swap conditional on its slot, each read of every slot taking the newest; shown to be the one conditional head specs/bucket models (FencedWrites, ReadsLinearizable), each rule with a mutation. specs/bucket says so; its models pass unchanged.
<!-- SECTION:FINAL_SUMMARY:END -->
