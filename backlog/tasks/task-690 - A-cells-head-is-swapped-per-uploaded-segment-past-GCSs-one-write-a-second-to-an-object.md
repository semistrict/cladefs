---
id: TASK-690
title: >-
  A cell's head is swapped per uploaded segment, past GCS's one write a second
  to an object
status: To Do
assignee: []
created_date: '2026-11-20 08:19'
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
- [ ] #1 A test with a bucket refusing a second write to a key within 1 s shows one cell writing 200 MB/s and one fsync-looping cell each swapping their head at most once a second, and their commits still complete
- [ ] #2 A 429 on one cell's head does not throttle other cells' head swaps on the host
- [ ] #3 A commit waiting for its upload waits at most ~1 s more than its PUTs for the swap naming it
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
