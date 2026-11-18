---
id: TASK-628
title: Each ordered commit recounts the open segment's extents under the state lock
status: To Do
assignee: []
created_date: '2026-11-18 15:35'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 1019000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), local-core review; verified against the code by independent reviewers. Severity: low.

**Where:** `crates/local_cell/src/segments.rs`: `rg -n -F 'windows.flat_map(|window| window.sealed.values()).map(|(_, len)| len).sum()' crates/local_cell/src/segments.rs`

LocalCell::wal_bytes() returns `state.wal.bytes() + state.segments.open_bytes()`. Both are recomputed on every call. Queue::bytes() sums every unacknowledged WAL segment. Segments::open_bytes() walks Window.sealed, a BTreeMap with one entry per extent sealed in the open segment since it opened (and in the first full segment), which only shrinks when the segment closes. The uploader's lag check calls it on every ordered commit while holding the uploader's progress mutex, and wal_bytes takes the store's state lock, which every read, write, commit and page-in of the store also takes. With small commits, the k-th commit in a segment does O(k) work: 32 MiB of 4 KiB extents is 8,192 entries, and up to 24,576 at segment_limit. Over a segment's life that is quadratic, roughly 4,000 map entries walked per commit on average. When uploads lag, Queue::bytes adds O(WAL segments), up to the commit lag bound, per commit. A running total would make both O(1).

**Trigger.** A SQLite or Fxfs cell commits ordered commits, each sealing a few small extents. OpenCell::commit_and_wait (Commit::Ordered) calls Uploader::wait_lag(epoch). That calls Shared::ask(epoch - max_lag, true), then within(), which evaluates `self.store().wal_bytes()?` because uploaded >= epoch - 4096 in steady state. wal_bytes locks the state, sums Queue::bytes() over the WAL, and then open_bytes() iterates every Window.sealed value. The image's committed() hook (storage/kept.rs) takes the same path. The cost grows with each commit until the segment closes (32 MiB or 60 s), then resets.

**Why the tests stay green.** tests/cell.rs only checks wal_bytes' value. tests/hot_path.rs times locate, write, free and views, but never a commit made through wait_lag while the open segment fills. No test bounds the per-commit cost of the lag check.

**Backlog.** RELATED: TASK-165.9 (Done) added the open segment's bytes to wal_bytes with no word on cost; TASK-42.11 (To Do) suspects wait_lag's state-lock contention on every ordered commit but not this walk.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The lag check's wal_bytes costs O(1) per ordered commit: over 20,000 one-block commits in one open segment (large segment_size and segment_age), its time per commit stays flat rather than growing with the commit index.
- [ ] #2 Running totals of the open segment's sealed bytes and of unacknowledged write-ahead log segments equal a recount after random seals, frees, closes and acknowledgements.
- [ ] #3 A regression test in local_cell's tests/hot_path.rs times wait_lag per commit across the 20,000 commits, asserts it flat, and goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
