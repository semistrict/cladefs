---
id: TASK-512
title: >-
  AES-256-XTS encrypts one block at a time: 336 µs of a 256 KiB store write on
  ubu1, 72% of it
status: Done
assignee:
  - '@claude'
created_date: '2026-11-18 02:40'
updated_date: '2026-11-18 04:41'
labels: []
dependencies: []
references:
  - crates/storage_xts/src/lib.rs
  - crates/extent_keys/src/xts.rs
priority: high
type: bug
ordinal: 898000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-433.9's probe of store-bench burst on ubu1 (Ryzen 9 8945HS, VAES): a 256 KiB write to an open segment spends 336 µs encrypting its raw sectors (0.78 GB/s) of 466 µs in all; on a Mac 65 µs. Since TASK-411 every store is encrypted, so every guest write pays it. perf: aes's x86_vaes512 backend is chosen, but Fuchsia's storage_xts (vendored, crates/storage_xts) calls encrypt_block for each 16-byte block, the tweak chained, so AES runs latency-bound, never the backend's parallel blocks (ParBlocks). Decryption (reads of open segments, framing's reads) is the same.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 storage_xts encrypts and decrypts through the backend's parallel blocks, its output unchanged (IEEE 1619 vectors and aws-lc cross-check pass)
- [x] #2 A 256 KiB raw write's encryption on ubu1 at least 4x faster than 336 µs, measured under fxjob --bench
- [x] #3 The change to the vendored Fuchsia crate documented where it occurs; scripts/upstream-changes.py --check passes
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. storage_xts: one generic xts_buffer runs groups of the backend's ParBlocksSize through encrypt/decrypt_par_blocks, the rest one at a time; in place, out of place and CTS all use it; ARM64's DC ZVA kept for out of place.
2. Tests: a mock cipher with ParBlocksSize 3 matches the one-at-a-time mock, in and out of place, and counts its parallel calls (hand mutant: groups 0 fails 7 cases); extent_keys's IEEE 1619 vectors and aws-lc cross-check; fxfs-crypto, extent_frame.
3. Measure on ubu1 (probed store-bench burst, fxjob --bench) against main.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
ubu1, fxjob --bench, medians rounds 2-6, probed store-bench burst: a 256 KiB write's encryption 334 -> 72 µs (RAM loop ext4), 336 -> 74 µs (zvol): 4.6x. Write in all 460 -> 243 µs (RAM), 468 -> 269 µs (zvol); the writer now waits for write-back (46-60 µs/write), the flusher's QD1 pwrites its bound (TASK-433.9). Mac: storage_xts 27/27, extent_keys + fxfs-crypto + extent_frame 131/131.

Verified: Mac workspace 2659/2659, Shuttle 37/37, clippy gate; ubu1 workspace 2973/2973, sim, Shuttle 37/37, loom (tree with TASK-433.9's frames and TASK-433.10). upstream-changes.py --check passes; the change documented in crates/storage_xts/src/lib.rs (xts_buffer's cladefs: comment). Final A/B: encryption 71 -> 63 µs per 256 KiB write beside frames.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Fuchsia's storage_xts ran each 16-byte block through encrypt_block, AES latency-bound: 334 µs of a 256 KiB store write's 460 on ubu1, since every store is encrypted (TASK-411). One xts_buffer now takes groups of the backend's ParBlocksSize through encrypt/decrypt_par_blocks (in place, out of place, CTS): 72 µs, 4.6x. Output unchanged (IEEE 1619 vectors, aws-lc cross-check); a parallel mock proves the batched path (hand mutant fails 7 cases). Measurements: docs/measurements/ubu1-burst-writes-2026-11-17.md.
<!-- SECTION:FINAL_SUMMARY:END -->
