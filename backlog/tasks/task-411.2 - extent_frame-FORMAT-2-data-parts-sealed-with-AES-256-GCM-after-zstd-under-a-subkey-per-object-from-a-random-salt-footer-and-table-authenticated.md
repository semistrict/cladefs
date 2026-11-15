---
id: TASK-411.2
title: >-
  extent_frame FORMAT 2: data parts sealed with AES-256-GCM after zstd, under a
  subkey per object from a random salt; footer and table authenticated
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-14 18:33'
updated_date: '2026-11-15 01:38'
labels: []
dependencies:
  - TASK-411.1
documentation:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
parent_task_id: TASK-411
type: task
ordinal: 799000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The bucket, the WAL and the part cache all hold framed objects, so encrypting parts at framing covers all three (doc-16, What is encrypted). The salt per object is needed because a fenced writer can hold different bytes for a segment key another epoch landed (doc-14), and nonces from the key alone would repeat.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A framed object with a key has no plaintext part bytes; reading it back with the key gives the data, with the wrong key or a flipped byte an error, never wrong data
- [ ] #2 Framing throughput with encryption is measured against without, on ubu1 via fxjob --bench, and the numbers are in the task
- [ ] #3 A FORMAT 2 reader refuses a FORMAT 1 object by naming its format, not as corrupt (doc-11 rule 1); no migration (the user's decision)
- [x] #4 An object of a cell with no key is written and read in the clear, its trailer saying so; each encryption test has a plaintext twin
- [ ] #5 A part moved to another index or another object fails authentication: its subkey comes from its object's salt (not the object's key, which a rebase's restamping may change, doc-14), and its associated data covers its index, encoding and lengths
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Throughput (AC #4), ubu1 under fxjob --bench (isolated cores 4-7), fxdax-examples frame-bench 256 MiB, one thread, best of 5, 2026-11-15 at 979ecd28f: text (compresses by half) pack 1075.7 MiB/s clear, 1036.3 sealed (-4%); unpack 2757.0 clear, 2520.8 sealed (-9%). noise (incompressible) pack 1723.1 clear, 994.0 sealed (-42%); unpack 9130.5 clear, 5425.1 sealed (-41%). Mac M5 Pro (loaded): text pack 1219/1010, unpack 2494/2052; noise pack 5128/2695, unpack 14014/4091. Sealing costs little where zstd dominates and ~40% where data is stored raw, still ~1 GB/s and more a thread.
<!-- SECTION:NOTES:END -->
