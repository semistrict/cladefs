---
id: TASK-684
title: >-
  A take picks each member's longest log by its bytes, unauthenticated: a member
  answering a longer, garbled copy hides an honest one, and commits told
  replicated are lost
status: To Do
assignee: []
created_date: '2026-11-20 03:05'
labels:
  - durability
  - peers
  - security
dependencies: []
references:
  - crates/local_cell/src/upload.rs
  - crates/local_cell/src/replicate.rs
priority: medium
type: bug
ordinal: 1065000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found 2026-11-19 writing TASK-680's review test. Shared::read_longest keeps, of each log, the copy of the member whose fence answered the greatest end, and replicate::trimmed then reads its records holding no key (wal::replica_records, LogRecord::decode with None). A copy whose frames are garbled past some point reads as a shorter log, its records past it gone, as a torn tail does; one longer than the honest copies but garbled wins the pick and hides them. A member, or anything on the unauthenticated peer network (doc-21: no authentication for now), can so make a take lose commits told replicated, silently, not only deny service as doc-21 says a hostile host can. The take holds the cell's keys by then (TASK-680's review added wal::replica_authentic for the gap case).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 read_longest prefers, of each log, the longest copy whose header and records the cell's keys find authentic, a member's copy that isn't counted as none
- [ ] #2 A test: two members, one answering a longer copy with a record garbled or MACed under another key; the take recovers every commit the honest one holds
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
