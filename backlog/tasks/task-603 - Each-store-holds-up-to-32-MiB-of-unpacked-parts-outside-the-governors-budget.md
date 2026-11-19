---
id: TASK-603
title: Each store holds up to 32 MiB of unpacked parts outside the governor's budget
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18 15:31'
updated_date: '2026-11-19 07:06'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 994000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), local-core review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/local_cell/src/framed.rs`: `rg -n -F 'const UNPACKED: usize = 32;' crates/local_cell/src/framed.rs`

Every LocalCell has its own Unpacked LRU of up to 32 decompressed or raw parts. Each is an Aligned of up to PART (1 MiB) plus 4 KiB, so up to about 32 MiB per store. Packed::unpack fills it on every read of a locally framed file's part (`unpacked.put(self.key, part.clone())`). Entries are keyed by (Framed id, part) and are never invalidated. Once a segment is acknowledged and reaped, or its Framed is dropped from Files, its parts can never be hit again, yet they stay until 32 newer puts push them out. A store whose later reads go to the bucket keeps them for life. This memory is not in cladefsd's governed_bytes() (arenas + held parts + pages), and govern() cannot evict it. It also duplicates pages the extent cache holds. AGENTS.md requires any caching on a data path to use local_cell::pages or a budgeted holder the governor counts. With hundreds of writer stores on a host, this is gigabytes outside the one budget, which stays held under memory pressure.

**Trigger.** cladefsd serves N writer stores. A guest maps or reads extents sealed in a segment that the uploader's interval closed and framed but the bucket has not yet acknowledged; this is common whenever uploads lag, as in the slow-bucket soak, or on a rebind after writeback. The path is ExtentCache page-in (cache.rs store.read), then LocalCell::read, read_parts, Located::Packed, read_packed, Packed::unpack, Unpacked::put. Followers' read_view takes the same route. Each store ends up holding 32 parts. After the segment is acknowledged and leaves, those parts remain referenced only by the LRU, and are never read or evicted, for the store's life.

**Why the tests stay green.** No test counts the Unpacked cache or checks that it empties or shrinks under govern(). The daemon's AnonBudget gives each writer 288 MiB of framing headroom (FRAMING = 2*(32 MiB + 64 MiB) + 64 MiB + 32 MiB), which absorbs 32 MiB per store, so the budget checks in the soaks and qualifications stay green.

**Backlog.** New. TASK-29.2 (Done) introduced the LRU and TASK-92.2 (Done) saw its stranded keys only as a hit-rate issue; TASK-433 and TASK-443.2 (In Progress) cover other holders outside the governor.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Once a segment is acknowledged and leaves the store, or its Framed is dropped, the store holds no unpacked parts of it.
- [ ] #2 governed_bytes() counts every store's unpacked parts, and govern() with a small allowance frees them.
- [ ] #3 A local_cell test writes and commits about 40 MiB of compressible extents, closes the segment (close_now), reads every extent, acknowledges the segment and asserts none of its parts stay unpacked, plus a daemon-level governed_bytes/govern test; both go red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
