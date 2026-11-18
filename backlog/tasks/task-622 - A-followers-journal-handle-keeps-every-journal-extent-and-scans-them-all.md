---
id: TASK-622
title: A follower's journal handle keeps every journal extent and scans them all
status: To Do
assignee: []
created_date: '2026-11-18 15:35'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 1013000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), fxfs-journal review; verified against the code by independent reviewers. Severity: low.

**Where:** `crates/fxfs/src/object_store/journal/bootstrap_handle.rs`: `rg -n -F 'self.extents.push(Extent { added_offset, device_range });' crates/fxfs/src/object_store/journal/bootstrap_handle.rs`

A read-only mount that replayed the whole journal keeps its JournalReader in `Journal::follower`. Each `read_on` runs `read_transactions`, which, at each Commit, `push_extent`s every new journal-file extent the writer's `make_room` added: 256 extents of 4 KiB per MiB of journal. `rewind(&committed)` keeps the handle, and nothing ever drops extents wholly behind the reader (`truncate_extents` and `discard_extents` only cut from an offset forward). `BootstrapObjectHandle::read_aligned` walks `extents` from the first one (`for extent in extents {`) to find the read offset, so every block read costs O(extents ever pushed). A follower of a busy writer gains about 10 MiB of guest-kernel memory per GiB of the writer's journal; an fsync-heavy writer produces ~14 GiB an hour, or ~140 MiB an hour. Each batch's reads slow down in proportion, until the follower guest runs out of memory. Followers are frozen (doc-8: no customer) but kept tested and working; a view is cut off only when stale, never for living long.

**Trigger.** 1. VM B mounts a follower of VM A's store. A runs an fsync-heavy workload, padding a 4 KiB journal block per fsync.
2. Every MiB of A's journal: make_room's transaction journals up to 256 new journal-file extent records.
3. Each COMMITTED notification -> follow() -> fetch() -> Journal::read_on -> read_transactions -> on Commit, reader.push_extent for each, appended to BootstrapObjectHandle::extents forever.
4. After G GiB of A's journal, B holds 262,144*G entries, and every read_block -> read_aligned iterates all of them before reaching the current offset. Per-batch CPU and resident memory grow linearly with A's uptime.

**Why the tests stay green.** Follower tests (follower.rs, crates/follower_sim, model tests) follow short histories of at most a few MiB of journal, and nothing measures the reader handle's extent count or per-fetch cost over a long run.

**Backlog.** Related: TASK-92.18 (Done) noted that read_aligned scans from the first extent but fixed only a mount's bounded replay. TASK-42.4 (Done) built read_on without trimming the handle.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 While a writer writes 64 MiB of journal and a follower follows each commit, the follower's journal handle holds no more extents than cover its journal checkpoint to the journal's end ((end - committed)/4096 plus preallocation). Its memory and per-read cost depend on the live journal, not on all the journal bytes the writer ever wrote.
- [ ] #2 A test in crates/fxfs's follower.rs tests asserts that extent count through a test accessor on Journal. It goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
