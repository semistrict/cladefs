---
id: TASK-633.4
title: >-
  Route every bucket request by the cell's bucket: per-bucket options, fetchers,
  budgets, peers and stats
status: Done
assignee: []
created_date: '2026-11-18 17:03'
updated_date: '2026-11-19 02:34'
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
- [x] #1 A resolver maps a cell ID to its bucket through the registry, each bucket opened with its own options; the URL-keyed BUCKETS map stays, mapping the host's bucket URL to its router
- [x] #2 Fetchers, recovery's checkpoints and a writer's moves read each key from its cell's bucket, each bucket with its own GET budget
- [x] #3 Peers serve and ask parts of every bucket in their registry
- [x] #4 Stats and /metrics count GETs and bytes per bucket number; tests read one store's inherited extents from a second bucket of another kind (file and S3 server)
- [x] #5 Parts of objects read from another cloud region's bucket are kept in the host's part cache and served by its region's peers exactly as its own bucket's are: the region's owner of a part fetches it from the remote bucket once
- [x] #6 Peers never cross a cloud region: every peer request names the asking host's home bucket number, a peer refuses any not its own, and a host asks no peer of another region whatever its list says; a test shows a peer of another home bucket refused and never asked
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Evidence (all in 82bb2aa37, on main): object_bucket::tests::requests_go_to_the_bucket_of_their_cell (routing, per-bucket stats), numbered_options_take_their_buckets_own_variables_first; children_in_one_bucket_read_their_parents_in_another (child in a file bucket, parent in the S3 test server reached with CLADEFSD_BUCKET_2_* options alone; recovery and reads of inherited extents from bucket 2; a part fetched once then served from the part cache by stored_part, what peers serve: 0 GETs, mutant without cache 2 GETs); fetch::tests::budgets_per_bucket_hold_up_no_other_buckets_gets; peers::tests::a_peer_of_another_region_is_never_asked_again, peers_serve_their_region_alone; cladefsd peers_of_another_region_are_refused_as_foreign (Linux). Peers serve any key through the host's routed fetcher (Serving::part -> stored_part), so every registry bucket's parts. /metrics: cladefsd_object_storage_bucket_gets_total{bucket}, _downloaded_bytes_total{bucket}. Runs at 82bb2aa37: macOS 2694/2694 + sim 41/41 + clippy; Linux ubu1 3023/3023 + sim + all parts. scripts/upstream-changes.py --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Every bucket request routes by its cell's bucket number: ObjectBucket::routed with the Registered resolver (registry copy, else the root's registry; each bucket opened with CLADEFSD_BUCKET_<N>_* over the shared environment, its identity checked). Fetchers take per-bucket GET budgets; stats and /metrics count per bucket number; peers are partitioned by region (home bucket + root identity in each request; foreign peers refused and never asked again) and serve any bucket's parts through the host's routed fetcher and part cache. Verified by the tests named in the notes, on macOS and Linux at 82bb2aa37.
<!-- SECTION:FINAL_SUMMARY:END -->
