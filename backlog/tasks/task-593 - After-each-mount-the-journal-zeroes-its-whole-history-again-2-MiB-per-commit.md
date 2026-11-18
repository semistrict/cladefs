---
id: TASK-593
title: 'After each mount the journal zeroes its whole history again, 2 MiB per commit'
status: To Do
assignee: []
created_date: '2026-11-18 15:30'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 984000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), fxfs-journal review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/fxfs/src/object_store/journal/position.rs`: `rg -n -F 'self.zero_offset = (zero_to > self.zeroed_to).then_some(zero_to);' crates/fxfs/src/object_store/journal/position.rs`

`JournalPosition::zeroed_to` starts at 0 in every mount. `Journal::new` uses `JournalPosition::default()`, and `replayed()` never sets `zeroed_to`. The first super-block written after a mount calls `superseded(old_start = loaded super-block's journal start S, ..)`, which sets `zero_offset = align_down(S)` with `zeroed_to == 0`. `make_room` then zeroes `zeroed_to..zeroed_to+MAX_ZEROED_PER_COMMIT` (2 MiB), one slice per user commit. So the next S/2 MiB commits each carry an extra journal-own transaction (journal.rs `let zeroing = inner.position.zeroing(MAX_ZEROED_PER_COMMIT);`). Earlier mounts already zeroed almost all of that range, so `handle.zero` finds nothing there, but the transaction is still built, applied and journaled (UpdateBorrowed + Commit). While `zeroing.is_some()`, `make_room` also skips its `paying` arm (journal.rs `let mut transaction = if extension.is_none() && zeroing.is_none() && pays {`). The transaction is then `Borrowed`, and `did_commit_transaction` charges 4x (`reserved_space_from_journal_usage`) the padding of the sync before it to `borrowed_metadata_space`. That is the metadata-reservation drain TASK-26.6.1 fixed, and it comes back for the whole catch-up window, pulling the reservation down to `METADATA_LOW_MARGIN` and forcing early compactions. S is the cell's cumulative journal offset, which never resets: every fsync pads a 4 KiB block, so an fsync-heavy guest adds about 14 GiB an hour, which is about 7,000 extra transactions per remount for each hour of history. Every fork of a version pays it again. Upstream zeroed `0..zero_offset` in one transaction, which is cheap over sparse ranges. Slicing it (TASK-139) without restoring `zeroed_to` at mount made the cost proportional to the filesystem's whole history.

**Trigger.** 1. A guest runs Postgres with an fsync per commit for hours. Each fsync pads the journal to a block, so the super-block's journal_checkpoint.file_offset S reaches tens of GiB.
2. The VM restarts, or the cell is forked or restored. FxFilesystem::open -> Journal::replay -> position.replayed(..) leaves zeroed_to = 0.
3. The first write_super_block runs (record_journal_tail for a renewed or forked tail, or the first compaction). superseded(S, start) sets zero_offset = Some(align_down(S)).
4. Every following commit runs pre_commit -> make_room: zeroing = Some(k*2MiB..(k+1)*2MiB). A borrowing journal transaction runs handle.zero, which deallocates nothing, then write_and_apply_mutations writes UpdateBorrowed+Commit. This repeats for S/2MiB commits (~20,000 for S = 40 GiB).
5. Each of those commits also borrows 4x the previous fsync's padding. should_compact's METADATA_LOW_MARGIN arm then fires every MIN_COMPACTION_BYTES (128 KiB) of journal, not every reclaim_size/2. Each early compaction flushes every dirty store and writes a super-block.

**Why the tests stay green.** No test remounts a filesystem whose journal start is past a few MiB and then looks at commits. test_commits_zeroing_old_space_stay_within_the_journal zeroes within one mount. position.rs's unit tests build positions with replayed_to(..), whose zeroed_to is 0, and never say where a mount's zeroing should start. The behaviour is correct, only slow, so crash and model tests stay green.

**Backlog.** No task claims the restart from 0. TASK-139 (Done) sliced the zeroing without restoring at mount how far it had reached, and that brings back TASK-26.6.1's (Done) padding borrow after every mount.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 After a remount or fork of a filesystem whose journal start is past 32 MiB, the journal's zeroing starts at or after the first extent the journal file still holds, not at offset 0. The zeroing transactions after a mount number at most the held journal's size divided by 2 MiB, not the journal start divided by 2 MiB.
- [ ] #2 100 fsync'd commits after such a remount leave borrowed_metadata_space no higher than 100 such commits on a freshly formatted filesystem.
- [ ] #3 A test in crates/fxfs/src/object_store/journal.rs commits and syncs until journal_checkpoint.file_offset is past 32 MiB, then remounts, runs force_compact, and asserts both criteria above. It goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
