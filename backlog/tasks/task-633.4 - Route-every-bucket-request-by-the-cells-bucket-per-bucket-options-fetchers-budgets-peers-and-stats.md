---
id: TASK-633.4
title: >-
  Route every bucket request by the cell's bucket: per-bucket options, fetchers,
  budgets, peers and stats
status: To Do
assignee: []
created_date: '2026-11-18 17:03'
updated_date: '2026-11-18 22:52'
labels:
  - bucket
  - multi-region
dependencies:
  - TASK-633.3
documentation:
  - >-
    backlog/docs/doc-23 -
    Cells-across-buckets-a-bucket-per-cloud-region-forks-that-span-them.md
parent_task_id: TASK-633
priority: high
type: feature
ordinal: 935000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Every bucket a process opens shares the process environment (`reach(url, std::env::vars(), &Client::DEFAULT)`: one AWS_REGION, one Azure account), the fetcher and GET budget are built over one bucket, and peers serve and ask only the host's bucket (`if bucket != self.bucket {`). A child whose parent lives in another bucket, maybe another cloud, needs each key fetched from its own bucket with that bucket's credentials (doc-23, Routing by bucket).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A resolver maps a cell ID to its bucket through the registry, each bucket opened with its own options; the URL-keyed BUCKETS map goes
- [ ] #2 Fetchers, recovery's checkpoints and a writer's moves read each key from its cell's bucket, each bucket with its own GET budget
- [ ] #3 Peers serve and ask parts of every bucket in their registry
- [ ] #4 Stats and /metrics count GETs and bytes per bucket number; tests read one store's inherited extents from a second bucket of another kind (file and S3 server)
- [ ] #5 Parts of objects read from another cloud region's bucket are kept in the host's part cache and served by its region's peers exactly as its own bucket's are: the region's owner of a part fetches it from the remote bucket once
- [ ] #6 Peers never cross a cloud region: every peer request names the asking host's home bucket number, a peer refuses any not its own, and a host asks no peer of another region whatever its list says; a test shows a peer of another home bucket refused and never asked
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
