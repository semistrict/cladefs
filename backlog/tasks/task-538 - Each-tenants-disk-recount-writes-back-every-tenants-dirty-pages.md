---
id: TASK-538
title: Each tenant's disk recount writes back every tenant's dirty pages
status: To Do
assignee: []
created_date: '2026-11-18 13:00'
labels:
  - extentd
  - performance
dependencies: []
priority: medium
type: bug
ordinal: 931000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Disk::recount (crates/cladefs/src/cladefsd/disk.rs) calls local_cell::pages::shared().write_back_all() before walking a tenant's directory, so that a write held dirty in the stores' page cache (TASK-433) is either counted as writing or found by the walk. The page cache is the whole process's, so each tenant's recount writes back the dirty pages of every store on the host. The recounter recounts every tenant every RECOUNT_EVERY (10 s), so a pass with T tenants writes back the whole cache T times, and a tenant refused ENOSPC that keeps writing triggers it up to 10 times a second (RECOUNT_GAP). Write-back that the cache would have deferred and coalesced is forced out, and other tenants pay for one tenant's accounting. The directory walk itself is cheap: a store's directory holds a handful of files (open segment, logs, closed segments not yet acknowledged). Related: TASK-520 and TASK-355 come from the same choice of counting by walking st_blocks rather than by what the daemon writes.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A recount writes back no store's pages outside the tenant it counts, or none at all
- [ ] #2 A test with two tenants shows that recounting one leaves the other's dirty pages dirty
- [ ] #3 The hostile cladefsd test and disk.rs's writes_not_done_stay_counted_across_walks still pass: no tenant is admitted past its limit
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
