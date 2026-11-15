---
id: TASK-411.2
title: >-
  extent_frame FORMAT 2: data parts sealed with AES-256-GCM after zstd, under a
  subkey per object from a random salt; footer and table authenticated
status: Done
assignee:
  - '@claude'
created_date: '2026-11-14 18:33'
updated_date: '2026-11-15 03:23'
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
- [x] #1 A framed object with a key has no plaintext part bytes; reading it back with the key gives the data, with the wrong key or a flipped byte an error, never wrong data
- [x] #2 Framing throughput with encryption is measured against without, on ubu1 via fxjob --bench, and the numbers are in the task
- [x] #3 A FORMAT 2 reader refuses a FORMAT 1 object by naming its format, not as corrupt (doc-11 rule 1); no migration (the user's decision)
- [x] #4 An object of a cell with no key is written and read in the clear, its trailer saying so; each encryption test has a plaintext twin
- [x] #5 A part moved to another index or another object fails authentication: its subkey comes from its object's salt (not the object's key, which a rebase's restamping may change, doc-14), and its associated data covers its index, encoding and lengths
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Throughput (AC #4), ubu1 under fxjob --bench (isolated cores 4-7), fxdax-examples frame-bench 256 MiB, one thread, best of 5, 2026-11-15 at 979ecd28f: text (compresses by half) pack 1075.7 MiB/s clear, 1036.3 sealed (-4%); unpack 2757.0 clear, 2520.8 sealed (-9%). noise (incompressible) pack 1723.1 clear, 994.0 sealed (-42%); unpack 9130.5 clear, 5425.1 sealed (-41%). Mac M5 Pro (loaded): text pack 1219/1010, unpack 2494/2052; noise pack 5128/2695, unpack 14014/4091. Sealing costs little where zstd dominates and ~40% where data is stored raw, still ~1 GB/s and more a thread.

AC evidence: extent_frame tests (31 at TASK-411.2, 33 after): sealed objects hold no 64-byte run of plaintext or zstd output; read back by part, whole and in place; no key, wrong key, flipped part byte (with and without CRC fixed), flipped tag, part moved to another index or object, tampered footer/table/salt all refused; FORMAT 1 refused by name ('a frame of format 1, which a reader of format 2 doesn't read'); plaintext twins. Throughput: frame-bench on ubu1 fxjob --bench (notes above). Verified 2026-11-15 at 594ff82e9 (origin/main merged): macOS scripts/test.sh workspace 2466/2466, shuttle, scripts (mutants 0 broken), go; Linux ubu1 under fxjob (target on /mnt/fxdax-scratch), every part: clippy, workspace 2752/2752, sim 63/63, shuttle, scripts, go, guest, firecracker. Only fxdax's own crates changed (extent_keys, extent_frame, extent_remote, extent_store, sqlite_extents, fxdax, fxdax-examples): no upstream deviation; scripts/upstream-changes.py --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
extent_frame FORMAT 2: parts compressed then sealed with AES-256-GCM under a subkey HKDF derives from the branch's DEK and a random per-object salt (not the object's key, which rebases change), nonce the part index, AAD index/encoding/lengths; tags in 33-byte table entries; footer and table HMAC'd; seals Clear/Sealed (and Authenticated for checkpoints, TASK-411.3) in the trailer; keyless readers check CRCs; restamp needs the key. Sealing costs 4-9% on compressible data, ~40% on incompressible (ubu1, ~1 GB/s+ a thread). Verified on macOS and Linux (see notes).
<!-- SECTION:FINAL_SUMMARY:END -->
