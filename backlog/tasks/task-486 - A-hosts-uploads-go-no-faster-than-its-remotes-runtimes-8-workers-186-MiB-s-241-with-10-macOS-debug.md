---
id: TASK-486
title: >-
  A host's uploads go no faster than its remotes' runtime's 8 workers: 186
  MiB/s, 241 with 10 (macOS debug)
status: To Do
assignee: []
created_date: '2026-11-16 07:45'
labels:
  - perf
  - review-2026-11-16
dependencies: []
priority: low
ordinal: 882000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Measured by TASK-449's review (cladefs remote::lifecycle::uploads_after_restores, macOS, 15 cores, debug build, the S3 server in memory over HTTP): 512 MiB committed after 8 restores uploads at 186 MiB/s on extent_store::upload::remotes (available_parallelism clamped to 2-8 workers, TASK-34), 235 MiB/s when block_on's calls had a runtime of 2 workers of their own beside it serving the connections they left in the pool, 241 MiB/s on one runtime of 10. So upload throughput follows the threads serving the connections' sockets, wherever they are. Not known: whether a release build on Linux against a real bucket (TLS) is bound the same way, or by the network; the clamp at 8 was chosen for many stores waiting on object storage, not for one store's catch-up.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Upload throughput against the remotes' runtime's size measured on Linux (ubu1) in a release build, over TLS if a TLS test server exists or against a real bucket
- [ ] #2 The runtime's size chosen from that measurement and the choice recorded in doc-7
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
