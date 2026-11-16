---
id: TASK-426
title: >-
  extent_store: Store::close returns while the framer still reads a part looked
  at ahead
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 05:35'
updated_date: '2026-11-16 03:01'
labels:
  - bug
  - extent_store
dependencies: []
priority: low
type: bug
ordinal: 818000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-423 (2026-11-15). Store::close waits for framings and the next log's preparation, then marks the store closed; looks ahead queued after that are skipped (Framer::to_see checks standing.is_closed()), but one already reading its part (Framer::see, holding no lock since TASK-400) reads on after close returns, possibly after close removed the raw file, and then hands a Hold to the shared HeldParts ledger. Benign for data (reads only; a Hold for a gone segment is dropped), but it breaks "a closed store touches its directory no more": a store opened on the same directory right after (store-bench's "opening: reading N bytes", any test measuring an opening's I/O) counts up to 1 MiB of the old store's read. Framing::looking (TASK-423) says when one is in flight.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Once Store::close returns, no thread of the store reads its directory: a test holding a look ahead mid-read (failpoint) shows close waiting for it
- [x] #2 No framing or commit waits for a look ahead because of it
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Store::close, once it marks the store closed (so the framer begins none of the looks queued, Framer::to_see), waits while Framing::looking, before it removes the raw files. Only close waits: framings and commits never do. Failpoint store::looking_ahead in Framer::see, after it lets the lock go. Test close_waits_for_a_look_ahead_reading (tests/store.rs) holds a look ahead mid-read and closes on another thread: without the wait close returns at once (fails 'closed, reading', Ok(false)); with it close returns only once released. extent_store 292/292 on macOS, clippy (plain and shuttle) clean, fmt clean.
<!-- SECTION:NOTES:END -->
