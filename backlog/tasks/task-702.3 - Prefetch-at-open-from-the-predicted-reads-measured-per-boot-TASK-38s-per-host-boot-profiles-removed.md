---
id: TASK-702.3
title: >-
  Prefetch at open from the predicted reads, measured per boot; TASK-38's
  per-host boot profiles removed
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-20 19:46'
updated_date: '2026-11-21 06:18'
labels:
  - sandbox
  - performance
  - cache
dependencies:
  - TASK-702.1
  - TASK-702.2
parent_task_id: TASK-702
priority: high
ordinal: 1105000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
A cold host's first fork of a version reads its base a part at a time (TASK-670: ~1.6 GETs in flight). With a prediction of its reads from recent evidence (the model TASK-702.1 chooses), the daemon can fetch the predicted parts at once as the fork opens, behind demand reads, so the guest boots at once and finds them local. The part cache suffices: the sandbox team measured a base on local disk alone as fast as one in memory (0.20 s), and it charges no tenant's arena. Measuring each boot's prediction makes drift visible and lets the next model correct it.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 When a fork of a version opens, the daemon fetches the predicted parts into the part cache through the host fetcher as read-ahead: demand reads go first, peers are asked first, under the fetch budget, many in flight; the fork never waits for it; a test shows demand ahead of prefetch and more than four parts in flight
- [ ] #2 Only extents the fork's own store holds are prefetched
- [ ] #3 Each boot's prediction is measured against its reads (parts predicted and read, predicted and unread, read and unpredicted), in stats
- [ ] #4 TASK-38's boot profiles (profile files, their recorder and replay threads) are gone
- [ ] #5 On the test host, a cold host's first boot of a version with evidence answers within 1.5x of a warm one, with cold and with warm peers, recorded in docs/measurements
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 scripts/upstream-changes.py --check passes
- [ ] #2 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. bucket_cell fetch: Fetcher::fetch_ahead(extent, location, offset): the part holding the offset, found as read finds it (following moves), fetched at Priority::Ahead (demand promoted past it, peers asked first, under the host budget), kept in the part cache, not unpacked; nothing if the cache or memory holds it. HostFetcher::fetch_ahead on the bucket runtime.
2. cladefs evidence: every guest opening of a fork of a version has a boot window (RECORDED_FOR) tracking the units its guest reads; the first fork of the version an interval also writes them as a recording (as now). At the window's end each boot's units are measured against the prediction it had: units predicted and read, predicted and unread, read and unpredicted, counters in stats (zpages, control).
3. Prefetch: once the prediction is read, a task on the bucket runtime goes through its units in order, those of extents the fork's own store holds sealed (LocalCell::extent), each unit's bucket location (LocalCell::locate's BucketData; local data skipped), fetching them through HostFetcher::fetch_ahead, 128 at once; aborted as the store closes or drops; the guest never waits for it. Prefetches counter counts parts fetched.
4. TASK-38 gone: profile.rs, the cache's Recorder hook, OpenCell::profile and replay, the profiling field; Opening.profiled renamed for what it now means (opened for a guest), open_without_profile with it; their tests turned into tests of the prefetch (forks prefetch their prediction; stores opened for the host prefetch nothing).
5. Tests: bucket_cell: demand reads go ahead of fetch_ahead's, and more than four parts in flight; cladefs: a fork's prediction prefetched into the part cache, only of extents its store holds; the measurement counters.
6. ubu1 python-sandbox e2e: a cold host's first boot of a version with evidence against a warm one, with cold and warm peers; docs/measurements.
7. CONTEXT.md, doc-7/doc-24 where they name boot profiles; upstream-changes.py --check.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From TASK-702.1 (2026-11-20): predict the units any of the last 8 recordings read, no score threshold, fetched in the order of each unit's earliest read in any of them (5x less modelled waiting than by mean time when one set serves two workloads, docs/measurements/ubu1-python-sandbox-2026-11-20T2043Z-dc6e5dff); concurrency matters (modelled waiting 16.5 s at 32 fetches at once, 7.4 s at 64, ~0 at 128, 30 ms a fetch).

Built (2026-11-21): bucket_cell Fetcher::fetch_ahead (the part holding an offset, moves followed as read follows them, fetched at Priority::Ahead, kept in the part cache, not unpacked; nothing without a cache or if held); HostFetcher::fetch_ahead. crate::evidence: every guest opening of a fork of a version has a boot (10 s window); one fork per host and interval also writes it as a recording; at the window's end each boot is measured against the prediction it had: stats predicted-read, predicted-unread, read-unpredicted (units). Prefetch: once the prediction is read, its units in order, those of extents the fork's store holds in the bucket (LocalCell::place), 128 at once under the host's 64-GET budget, demand first; aborted as the store goes; prefetches counts parts fetched. TASK-38 gone: profile.rs, the cache's recorder hook, OpenCell::profile and replay; Opening.profiled is Opening.guest, open_without_profile open_for_host; the two profile tests became forks_of_a_version_fetch_their_prediction_ahead and stores_worked_on_here_fetch_nothing_ahead; fs-bench's fork-profiled case became fork-predicted (forks of one version of the base). Measurement harness: CLADEFS_PYSANDBOX_COLD adds a cold host's first boot after the rounds with cold peers (every host restarted) and with warm peers (one restarted). Mac: changed crates 1163/1163.
<!-- SECTION:NOTES:END -->
