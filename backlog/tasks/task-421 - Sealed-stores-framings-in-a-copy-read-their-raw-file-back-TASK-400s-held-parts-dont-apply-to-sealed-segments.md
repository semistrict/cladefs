---
id: TASK-421
title: >-
  Sealed stores' framings in a copy read their raw file back: TASK-400's held
  parts don't apply to sealed segments
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 02:37'
updated_date: '2026-11-15 11:11'
labels:
  - performance
  - encryption
dependencies: []
references:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
priority: low
ordinal: 812000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-400 (c2daf7630) packs a clear segment's parts as they are written whole and holds them in memory, so a framing in a copy reads none of its raw file back, which on a host short of memory waited 1.6-3.5 s on a busy disk. The merge of main into the TASK-411 work (a3adea64e) left sealed stores out: a sealed segment has no look ahead, so nothing is held, and its framing reads every part back through RawFile, decrypting. Doing it for sealed stores means packing and sealing at write time: the framed object's key (Packer::new(sealing) draws its salt and ObjectKey at framing) would have to exist when the segment opens and follow it through the framer's queue, the packing threads, eviction and a framing run again, each part's nonce (its index) never reused for other bytes. doc-16, What TASK-411.5 made, Framing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A sealed store's framing in a copy reads none of its whole parts back from its raw file when they are held (framing_reads counts only its last part), as a clear store's does
- [x] #2 No plaintext reaches disk; parts held in memory are sealed or freed as the governor evicts them; each framed object's key and part nonces are unique
- [x] #3 The framed bytes of a sealed segment open to its data whether its parts were held, evicted or read back, tested
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Settled by TASK-411.23 (doc-16, What TASK-411.23 made): sealed segments look ahead and hold parts packed in the clear in memory; a framing in a copy seals them under its object's key drawn at framing (PackedPart::sealed).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done in TASK-411.23 (02302b951). Packing ahead keeps parts in the clear in memory (HeldParts; memory holds plaintext anyway, doc-16 Goals) and seals them at framing, so no object key exists before the framing and each nonce (part index) is used once under a fresh salt; held parts never leave memory, evicted ones are dropped and read back through RawFile. Evidence: extent_store upload::encrypted::sealed_stores_hold_parts_packed_ahead_and_seal_them_framing (sealed and clear hold alike; framing reads back only the 4 KiB last part; no plaintext in any file; reads back), framing::tests::sealed_raw_files_parts_are_seen_as_stored_and_held_as_data (held or not, the sealed packed parts equal parts packed sealed fresh), segments::tests::sealed_segments_parts_are_held_ahead_and_sealed_framing (0 bytes read back, Seal::Sealed, opens under the key); extent_frame parts_packed_in_the_clear_seal_as_parts_packed_sealed. ubu1 sandboxes sealed: framing_read 0.13-0.14 GiB a host, slowest framings 111/129 ms.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Settled by TASK-411.23: sealed stores look ahead and hold parts packed ahead in the clear in memory, sealed under the framed object's key at framing; a sealed framing in a copy reads back only what was not held. Tests listed in notes; Linux workspace nextest (DoD #1) not run.
<!-- SECTION:FINAL_SUMMARY:END -->
