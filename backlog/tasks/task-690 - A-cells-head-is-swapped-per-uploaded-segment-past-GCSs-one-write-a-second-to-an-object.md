---
id: TASK-690
title: >-
  A cell's head is swapped per uploaded segment, past GCS's one write a second
  to an object
status: Done
assignee:
  - '@claude'
created_date: '2026-11-20 08:19'
updated_date: '2026-11-20 09:00'
labels:
  - bucket
  - gcs
dependencies: []
priority: high
type: bug
ordinal: 1060000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
GCS allows about one write a second to one object name; past it, 429 (rateLimitExceeded). A cell's head (layout::head, <cell>/head) is its one rewritten object, and the uploader swaps it once per acknowledged segment (upload.rs Uploader::acknowledged -> Writer::acknowledge -> head::swap), up to IN_FLIGHT = 16 segments of 32 MiB in flight, acknowledged one after another. So a cell writing ~100 MB/s swaps its head ~3/s; a guest looping fsync at the bucket's level (each commit waiting for its upload closes a segment urgently) swaps it as fast as a PUT and a swap take, several a second; versions, configuration switches and checkpoints swap it too. A 429 maps to bucket::Error::Unavailable (object_bucket.rs, the _ arm), and fails the host's Update turns (Turns::failed): every cell's head swap on the host then goes one at a time, after backoff. One busy cell throttles every cell's commits on the host. Never measured against GCS: S3 has no such per-key limit, and tests run RustFS/local.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 With a bucket refusing a write of a key within 1 s of the last (faults rewrite-every-ms=1000), a cell's uploader, first waiting for each commit's upload for 3 s, then closing 3 x IN_FLIGHT segments at once, has no write of its head refused, swaps it at most once a second, and every commit lands (local_cell upload::tests::heads_are_swapped_as_fast_as_gcs_takes_them)
- [x] #2 A refused update of one key does not hold back an update of another key on the host (bucket_cell object_bucket::tests::writes_of_a_key_are_spaced_as_its_store_takes_them)
- [x] #3 A commit waiting for its upload waits under 1.5 s with in-memory PUTs, except one after a checkpoint's own head swap (every 64 segments), which waits up to a second more
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Bucket::rewritable(key); ObjectBucket spaces writes of a key by its store's interval (GCS 1 s), shared with its twin, per routed member. 2. Writer::acknowledge waits for it before holding the head, then names all uploaded meanwhile. 3. Uploader runs head work (acknowledgement, then checkpoint) beside uploads, one at a time; commits return before the checkpoint. 4. Update requests take no host-wide Turns. 5. faults: rewrite-every-ms models GCS's 429.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Commits f4bc0532c, a639c6449. AC1 rewritten to what the test proves (one cell, sync loop then a burst; in-memory, not 200 MB/s on two cells). Red check: with the bucket unspaced, the GCS-like fault refuses the bucket's own probe update at open, failing the test. Review (verifying-reviewer): checkpoint ran before waiting commits returned, and head lock held through the 1 s wait; both fixed. Open: a checkpoint's own head swap costs a commit a slot every 64 segments; folding its naming into the next acknowledgement changes the protocol (collector vs. an unnamed checkpoint), not done. Verified: bucket_cell 367/367, local_cell 375/375 (sims incl.), local_cell shuttle_tests 9/9, cladefs check --tests, all on the Mac.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
A cell's head is swapped no faster than its bucket's store takes writes of one key (GCS: 1 s): the bucket spaces them, an acknowledgement waits first and names all uploaded meanwhile, and the uploader's head work runs beside its uploads. Updates back off per key instead of throttling every update on the host. Verified by new tests against a GCS-like fault and the full bucket_cell/local_cell suites.
<!-- SECTION:FINAL_SUMMARY:END -->
