---
id: TASK-421
title: >-
  Sealed stores' framings in a copy read their raw file back: TASK-400's held
  parts don't apply to sealed segments
status: To Do
assignee: []
created_date: '2026-11-15 02:37'
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
- [ ] #1 A sealed store's framing in a copy reads none of its whole parts back from its raw file when they are held (framing_reads counts only its last part), as a clear store's does
- [ ] #2 No plaintext reaches disk; parts held in memory are sealed or freed as the governor evicts them; each framed object's key and part nonces are unique
- [ ] #3 The framed bytes of a sealed segment open to its data whether its parts were held, evicted or read back, tested
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
