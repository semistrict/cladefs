---
id: TASK-702.4
title: Peers make a cold cluster's first boot slower than no peers
status: To Do
assignee: []
created_date: '2026-11-20 19:47'
updated_date: '2026-11-21 00:46'
labels:
  - sandbox
  - performance
  - cache
dependencies: []
parent_task_id: TASK-702
priority: medium
ordinal: 1106000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The sandbox team peered two daemons on one machine (no network cost). On a base neither had read, the first host's first `codex --version` took 4.7-5.6 s, against 3.6 s without peers: half its parts waited on an owner fetching them from the bucket itself (149 bucket GETs instead of ~300). With a peer already holding the base: 1.6-2.0 s. While waiting on the owner the asker holds a slot of its own fetch budget, and the owner fetches as a new demand read (crates/bucket_cell/src/fetch.rs, `Fetching::part`, `stored_part`). They propose fetching from the bucket when the owner lacks the part, and telling the owner; that gives up the deduplication peers exist for (TASK-406.2).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Where the extra time goes on a cold cluster's first boot with peers is measured on the test host and recorded in this task
- [ ] #2 A cold cluster's first boot with peers is no slower than without peers, measured, without raising bucket GETs above one per part for the cluster when peers are warm
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 scripts/upstream-changes.py --check passes
- [ ] #2 Deviations from upstream documented where they occur
<!-- DOD:END -->
