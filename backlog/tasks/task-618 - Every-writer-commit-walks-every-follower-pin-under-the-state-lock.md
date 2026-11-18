---
id: TASK-618
title: Every writer commit walks every follower pin under the state lock
status: To Do
assignee: []
created_date: '2026-11-18 15:34'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 1009000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), followers review; verified against the code by independent reviewers. Severity: low.

**Where:** `crates/local_cell/src/views.rs`: `rg -n -F 'pins: live().flat_map(|follower| follower.pins.iter().copied()).collect(),' crates/local_cell/src/views.rs`

Followers are supposed to cost writers nothing. TASK-92.29 made a lagging view's commit cost O(what it lets go of), but pins were left out of that fix. On every commit, LocalCell::publish holds the state lock and calls `Views::publish`. That call runs `keeping_of`, which builds a fresh HashSet of every pin of every live view. It then calls `state.trim_kept`. In `KeptExtents::trim`, every released kept entry freed at or before the oldest view's commit and still pinned stays in `released`, so each commit scans it again (`.filter(|(_, extent)| !Self::views_keep(...))`). Next, `self.for_views = self.views_bytes(keeping)` walks every pin again with a HashMap lookup each. Each WAL acknowledgement repeats the same work through `self.views.keeping()`. So while a follower pins P extents (the extents of deleted files its readers hold), every writer commit pays about 3·P hashing operations and one HashSet allocation, all while holding the state lock that the writer's writes, frees and commits need. TASK-92.29 brought a commit's store phase down to about 0.5 µs. With 10k pins it becomes hundreds of µs, which grows with pins, not with what the commit changes.

**Trigger.** Step 1. A follower reader holds open a deleted file of 10k+ extents (see the previous finding), so its view pins 10k extents for as long as the reader keeps it open. Step 2. The writer VM runs an fsync-heavy workload such as pgbench. Step 3. Each COMMIT's publish rebuilds the 10k-entry pin set, rescans the 10k pinned kept entries in trim and sums them again in views_bytes, all under the state lock. The writer's commit latency and throughput drop for as long as the follower keeps the file open.

**Why the tests stay green.** store-bench lagging-view (TASK-92.29's measurement) uses a view with no pins. local_cell/tests/views.rs and the follower simulation check that pins are correct, not what they cost per commit. TASK-42.11's pgbench runs with followers that hold no deleted files.

**Backlog.** Related: TASK-92.29 (Done) bounded a lagging view's rescans of frees and seals per commit but left pins out. TASK-42.11 (To Do, frozen) traces the follower slowdown to re-gathers and the wait_lag lock, not pins.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 With a follower view pinning 50k extents freed before its commit, the writer's per-commit work under the state lock does not grow with the number of pins. That covers publish, trim and the view byte count, and also each write-ahead log acknowledgement. Measured: trim's scanned entries and the keeping-set entries built per publish match the 0-pin run, or 1,000 commits take no longer than the 0-pin run within noise.
- [ ] #2 A local_cell test or store-bench case runs 1,000 commits with 50k pins and with 0 pins and asserts the bound. It goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
