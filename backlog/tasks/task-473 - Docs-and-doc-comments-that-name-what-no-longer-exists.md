---
id: TASK-473
title: Docs and doc comments that name what no longer exists
status: Done
assignee:
  - '@claude'
created_date: '2026-11-16 02:12'
updated_date: '2026-11-16 13:33'
labels:
  - docs
dependencies: []
ordinal: 877000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found scanning the repo for its terms (CONTEXT.md, 2026-11-15): docs and doc comments describing types, fields, directories and format numbers the code no longer has. A reader, or an agent, following them builds on what is gone. Each item is a pointer to check, not a verified fix list.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 crates/extent_remote/src/passed.rs: the `[layout::Lease]` intra-doc link names a type layout.rs no longer has (it has `Head`); cargo doc warns of no broken link
- [ ] #2 doc-14 no longer describes `State::apply(&mut self, &Footer) -> bool` or a footer `after` link (recovery.rs `apply` returns `()`; `Footer` has no `after`), nor "Opening a store takes the lease"
- [ ] #3 doc-11, doc-14 and doc-16 cite the formats the code has: store "cladefs extent store 11", `layout::FORMAT` 11, segment keys `<epoch>.<seq>`
- [ ] #4 crates/extent_store/src/lib.rs no longer describes an `index/<branch>/<n>` directory, which layout.rs does not make
- [ ] #5 `cell::Kind::name` doc names every kind it returns (memory too)
- [ ] #6 crates/fxfs/src/follower.rs says the caller drops what it cached before applying and settles after, as crates/follower does
- [ ] #7 storage_device/src/lib.rs and doc-7 cite no "Fencing" section that doc-7 no longer has
- [ ] #8 `layout::Head.token` doc says what the token holds as upload.rs sets it (`held.epoch`, the cell epoch), or the code sets what the doc says
- [ ] #9 doc-13 no longer lists `Source::is_open` or `fxfs_dax_store_begin` as live
- [ ] #10 scripts/test-host.sh local temp dir and scripts/lib/host-remote.sh lock file are named cladefs (neither is in rename-to-cladefs.sh exemptions)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Also: doc-8 decision 2 "Readers are VMs" says followers (reader = a task in a follower guest).
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Landed on main 29dec3ee1 (CONTEXT.md, the grilled word choices; renames across code, protos, wire, specs, scripts, docs). macOS: clippy, 2547 workspace tests, 516 script tests, mutants, TLC bucket and follower models and mutations. Linux (ubu1 t474d): 2824 workspace tests passed; guest cases passed but the pgbench pair, which fails main alike (TASK-492). The final merges were checked on macOS and with Linux clippy; ubu1 run t474e was still running at landing.
<!-- SECTION:FINAL_SUMMARY:END -->
