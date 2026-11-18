---
id: TASK-573
title: Every collector pass in full re-reads the trailer of every live segment
status: To Do
assignee: []
created_date: '2026-11-18 15:27'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 964000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), collector review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/bucket_cell/src/collector.rs`: `rg -n -F 'trailers.learn(bucket, &ours.iter().map(|(meta, _)| *meta).collect::<Vec<_>>()).await?;' crates/bucket_cell/src/collector.rs`

TASK-293 designed the data-length check as a cache 'in the Collector by key, version and listed size, as objects never change', and the Trailers doc still says each is read once. Since TASK-395.4 the collector remembers nothing between passes: collect_held, mark_held and dry_run_cell each start from Remembered::default(), so the trailer cache lasts one pass. Pass::compactable collects every named segment that the recovery did not read and that has no moves (that is, all live data covered by the newest checkpoint), plus every uncompacted moves object, and calls trailers.learn before below() is evaluated. So every pass in full makes one ranged GET per such object, every sweep, although the objects never change. A written cell (a running VM) changes between sweeps, so its pass is always in full; its live data spans hundreds to thousands of 32 MiB segments (checkpoint every 64 segments). The cost is O(live segments) GETs per cell per sweep, against O(objects/1000) LIST pages for its listing. With compact_below = 0.0, below() is always false, yet the reads still happen. They also happen in dry runs, and for encrypted cells whose KEK cannot be had, before compacting() clears the list.

**Trigger.** 1. A running VM's cell with N segments holding live extents under its newest checkpoint (e.g. a 50 GB disk is about 1,600 segments). 2. The VM writes between sweeps, so the cell's listing digest changes and Collector::look returns Changed. 3. collect_held: Remembered::default(), then survey, then pass.compact, then compactable(): all N segments are collectable (not touched, not ahead, no moves), and trailers.learn issues N GetRange(Suffix(TRAILER)) requests. 4. The next sweep does the same N reads again. With `cladefs-gc --compact-below 0` the same N reads are made and nothing can be compacted.

**Why the tests stay green.** Request-count tests pin LISTs (passes_list_their_cell_once...), dry-run footer GETs over versioned cells (whose segments are all touched, so no trailers), and idle looks. sweeps_report_what_the_bucket_counts only compares the sweep's own count with the bucket's. No test runs two passes in full over a cell with checkpointed live segments and counts GetRange.

**Backlog.** Regresses TASK-293 (Done), which promised compaction 'without a GET of every named object each pass', with trailer data lengths cached across passes because objects never change. TASK-395.4 (Done) made the collector remember nothing between passes, which undid that cache. TASK-412 (To Do) covers fork-point states, not trailers.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A pass in full over a cell whose checkpointed live segments have not changed since the last pass makes no trailer GetRange for them. Its trailer reads scale with the segments new since the last pass, not with all live segments (about 1,600 for a 50 GB cell).
- [ ] #2 A pass with compact_below 0.0 makes zero trailer reads, in dry runs and for cells whose KEK cannot be had as well.
- [ ] #3 A collector test on ObjectBucket writes 40 mostly-live segments, checkpoints and runs collect_cell. It then commits one more segment, runs collect_cell again, and asserts the second pass's GetRange count excludes the 40 unchanged segments. A second test runs a compact_below 0.0 pass and asserts zero trailer reads. Both go red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
