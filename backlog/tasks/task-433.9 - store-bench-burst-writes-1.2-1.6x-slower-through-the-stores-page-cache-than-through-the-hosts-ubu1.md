---
id: TASK-433.9
title: >-
  store-bench burst writes 1.2-1.6x slower through the stores' page cache than
  through the host's (ubu1)
status: To Do
assignee: []
created_date: '2026-11-16 15:47'
labels: []
dependencies: []
references:
  - crates/extent_store/src/pages.rs
parent_task_id: TASK-433
priority: medium
type: bug
ordinal: 886000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Left by TASK-433.8, which fixed the burst commit (now at or under the host page cache's). store-bench burst on ubu1, fxjob --bench, medians rounds 2-6, 0ef6fa1b4's code (host page cache) vs main 3e0256500: a 256 KiB write 311 vs 368 us on ext4 over a RAM-backed loop device, 294 vs 467 us on ext4 on the scratch zvol (docs/measurements/ubu1-burst-page-cache-2026-11-16.md). Not page zeroing (pages a write fills whole made unzeroed bought nothing, 2ddfa1dcb, dropped). Candidates, unmeasured: write_runs copies a run (1 MiB) into its buffer under the file's pages lock, which a writer of the same file waits on; one write-back in flight at a time (QD1); a BTreeMap insert and a 4 KiB allocation per page.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A profile (perf, lock waits) names where a burst write's time goes beyond the host page cache's
- [ ] #2 Burst writes on ubu1 within 10% of 0ef6fa1b4's, an A/B under fxjob --bench, each side its own target
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
