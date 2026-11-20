---
id: TASK-685.1
title: >-
  Spike: cladefs under --cfg madsim, two simulated daemons, a cell made on one
  and taken on the other
status: Done
assignee:
  - '@claude'
created_date: '2026-11-20 03:32'
updated_date: '2026-11-20 04:50'
labels:
  - testing
  - simulation
dependencies: []
parent_task_id: TASK-685
ordinal: 1067000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-24 step 1. Build the workspace with RUSTFLAGS="--cfg madsim" and tokio/tonic renamed to madsim-tokio/madsim-tonic, boot two daemons as madsim nodes over an in-process bucket, make a cell on one and take it on the other. List every break (threads, runtimes, block_in_place, Unix sockets, statics, std::fs, crates that use real tokio internally) with counts; size the later steps by it; if madsim can't host cladefsd, say why and rewrite doc-24 for the in-house fallback.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A written result in doc-24 (What the spike found) with the list of breaks and the decision to go on with madsim or not
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Landed d9f4438bd..3efa19eda. Verified: macOS workspace 2812/2812; ubu1 under fxjob clippy + workspace + sim 3122/3124, the 2 failures known flakes (TASK-677 under load, TASK-650 abort at exit), both passing alone after; the madsim test (RUSTFLAGS='--cfg madsim' CARGO_TARGET_DIR=target/madsim cargo nextest run -p bucket_cell --test madsim): 20,000 seeds in 3.7 s, MADSIM_TEST_CHECK_DETERMINISM over 200 seeds green.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
madsim hosts cladefs: renaming tokio/tonic to madsim's leaves normal builds unchanged; under --cfg madsim the bucket protocol runs with hosts as nodes (bucket_cell/tests/madsim.rs), seeds replay. cladefsd's own boot is what's left (env-configured sync main, threads, Unix sockets, statics, Linux page store): doc-24's What the spike found, and step 2 added (TASK-685.9). Decision: go on with madsim.
<!-- SECTION:FINAL_SUMMARY:END -->
