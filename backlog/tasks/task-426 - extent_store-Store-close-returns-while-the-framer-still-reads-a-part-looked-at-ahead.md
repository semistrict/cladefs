---
id: TASK-426
title: >-
  extent_store: Store::close returns while the framer still reads a part looked
  at ahead
status: To Do
assignee: []
created_date: '2026-11-15 05:35'
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
- [ ] #1 Once Store::close returns, no thread of the store reads its directory: a test holding a look ahead mid-read (failpoint) shows close waiting for it
- [ ] #2 No framing or commit waits for a look ahead because of it
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
