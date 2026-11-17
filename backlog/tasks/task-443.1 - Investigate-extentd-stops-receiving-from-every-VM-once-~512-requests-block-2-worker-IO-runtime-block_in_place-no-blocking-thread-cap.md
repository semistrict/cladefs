---
id: TASK-443.1
title: >-
  Investigate: cladefsd stops receiving from every VM once ~512 requests block
  (2-worker IO runtime, block_in_place, no blocking-thread cap)
status: To Do
assignee: []
created_date: '2026-11-15 12:24'
updated_date: '2026-11-16 22:52'
labels:
  - perf
  - review-2026-11-15
dependencies: []
documentation:
  - docs/reviews/2026-11-15-sandbox-performance.md
parent_task_id: TASK-443
priority: high
ordinal: 835000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/cladefs/src/cladefsd/server.rs builds the IO runtime with worker_threads(2) and no max_blocking_threads, and serves every request in tokio::task::block_in_place (8 a connection, WORKERS). A READ of data not in memory sleeps in futures::executor::block_on(bring_in) (crates/cladefs/src/cache.rs); durable and lagging commits wait likewise. At tokio's default cap of 512 blocking threads the worker hand-off queues and no connection's requests or faults are received: ~65 VMs with 8 cold reads each, or a slow bucket with a few hundred VMs. Three reviewers found it independently; the stall itself rests on tokio 1.53.1's behaviour at its cap, unconfirmed. TASK-92.20 fixed this for FAULT alone. It contradicts doc-12: a VM needing only local data should go on. Found by reading the code, not measured (docs/reviews/2026-11-15-sandbox-performance.md).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A test or run with 80+ connections each holding 8 READs of remote data behind a gated or throttled bucket, plus one connection reading local data: that connection's request latency and the daemon's thread count by name are recorded
- [ ] #2 The notes give the measured cost and the workload size at which a customer would feel it; a fix, if needed, is filed as its own task
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
