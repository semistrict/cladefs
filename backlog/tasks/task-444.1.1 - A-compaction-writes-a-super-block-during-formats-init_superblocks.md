---
id: TASK-444.1.1
title: A compaction writes a super-block during format's init_superblocks
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 14:12'
updated_date: '2026-11-15 14:25'
labels:
  - bug
dependencies: []
parent_task_id: TASK-444.1
priority: high
ordinal: 861000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the journal's trace validation (TASK-444.1): on a filesystem formatted with a small reclaim size (the model test's every-cut sequences, SHORT_RECLAIM), the flush task starts a compaction while format's init_superblocks is writing its second super-block: two write_super_block calls overlap (Pad c=4, then the compaction's Pad c=11 and Save c=11, then format's Save c=4: the older super-block committed last, both of the same generation, possibly to the same instance). write_super_block says callers must not overlap; force_compact was fixed for this in TASK-149.5, whose overlap left no readable super-block. init_superblocks takes no compaction slot. Replay: the trace of model_tests::journal_trace for Setting::awkward(16811558461824683114, ROOMY).cutting_everywhere() with [Mkdir(2), Open(9)] fails TLC at the second Pad; the pinned Rust test format_writes_its_super_blocks_alone checks the same without TLC.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A test replaying the overlap (super-block writes one at a time in the journal's trace) fails before the fix and passes after
- [ ] #2 Every caller of write_super_block holds the compaction slot, by construction
- [ ] #3 Notes per AGENTS.md: blind spot, class test, look-around
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
How found: the journal's trace validation (TASK-444.1). Three of the traced every-cut sequences stopped TLC at a Pad while pc was 'flushed': format's second init_superblocks write had padded and flushed, and the flush task, started by format's first commit, compacted the journal of SHORT_RECLAIM and saved its super-block (checkpoint 11) before format saved its own (checkpoint 4), of the same generation.
Blind spot: nothing checked that super-blocks are written one at a time. The model test checks what cuts leave, and the in-memory header regressed along with the device's, so replay still found what it needed in these runs; TASK-149.5's overlap (force_compact) was fixed at the one caller, by hand, and init_superblocks and record_journal_tail never took the slot.
Class test: every traced sequence checks super-blocks are written one at a time (SequenceTrace::super_blocks_one_at_a_time, in moments, without TLC), and TLC's trace check does (Pad requires write_super_block idle). Pinned: model_tests::journal_trace::format_writes_its_super_blocks_alone (seed 16811558461824683114, [Mkdir(2), Open(9)]): red with init_superblocks not taking the slot ('step 19: a super-block at 9181 begun as one at 4348 was being written'), green with the fix.
Structure: the class is made impossible: write_super_block takes a SuperBlockSlot, the token of compaction_running, which the flush task's compactions, force_compact, init_superblocks and record_journal_tail take (super_block_slot waits for it; dropping it frees it and wakes waiters). A caller without one doesn't compile.
Look-around: the four callers of write_super_block are all of them; record_journal_tail (mount after replay, the guest's thaw) was unguarded too, safe only because compactions are paused there, which force_compact ignores: now it takes the slot.
<!-- SECTION:NOTES:END -->
