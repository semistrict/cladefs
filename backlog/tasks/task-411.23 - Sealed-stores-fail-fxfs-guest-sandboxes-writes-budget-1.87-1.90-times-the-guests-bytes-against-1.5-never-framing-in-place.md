---
id: TASK-411.23
title: >-
  Sealed stores fail fxfs-guest-sandboxes' writes budget: 1.87-1.90 times the
  guests' bytes against 1.5, never framing in place
status: To Do
assignee: []
created_date: '2026-11-15 05:57'
labels:
  - performance
  - encryption
dependencies: []
references:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
parent_task_id: TASK-411
priority: high
ordinal: 817000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-411.15 (ubu1, 2026-11-15, prebuilt artifacts of 04d5b312, scripts/sandboxes.sh short): with the run's stores sealed (test-e2e.sh's FXDAX_KMS), fxfs-guest-sandboxes fails its check 'writes: the stores wrote 1.90 times what the guests did, past 1.5' (and 1.87 on a second run), and on the first run also 'framings: host-a framed 1 segments slower than 1000 ms, the slowest 1093.9 ms; host-b 2, the slowest 1080.6 ms'. The same artifacts with the stores in the clear (a KMS holding no KEK) pass: 1.16 times. Cause: doc-16 (What TASK-411.5 made, Framing) has sealed stores never frame in place (segments_framed_in_place=0 against 55-58 in the clear), so every segment is written twice, its XTS raw file then its GCM-sealed framed copy: written_framed_bytes 3.7-3.8 GB per host sealed against 0.32-0.36 GB clear, for about 5 GB raw. TASK-421 (framings read their raw file back) is the read side of the same choice. scripts/qualify.sh runs this case (long mode), so a qualification of an encrypted build fails its sandboxes part until this is settled. Either the budget is a clear store's and sealed stores get their own, said why, or sealed segments' framing stops rewriting what was written (e.g. sealing parts as they fill, TASK-421's packing ahead, so the framed copy is written once and the raw file's pages drop). Main (no encryption) passes; the e2e suite (--tag e2e) passes sealed, its daemons' budgets (scripts/budgets.py) holding.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 fxfs-guest-sandboxes passes sealed on ubu1 (scripts/sandboxes.sh short, then long under scripts/qualify.sh), its writes and framings checks holding, or its budgets for sealed stores changed with the measured reason
- [ ] #2 Write amplification of sealed and clear stores measured side by side on the same build and recorded in doc-16
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
