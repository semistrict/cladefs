---
id: TASK-648
title: >-
  A fork of a version right after its writer's connection drops can fail EBUSY:
  opened_version's sync reopens a store still closing
status: Done
assignee:
  - '@claude'
created_date: '2026-11-18 20:38'
updated_date: '2026-11-19 00:16'
labels:
  - cladefsd
  - fork
  - flaky
dependencies: []
priority: medium
type: bug
ordinal: 1028000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen on ubu1 (2026-11-18, TASK-633.9's runs) under full-suite load, never alone: cladefsd::tests::hostile::page_ins_in_flight_are_bounded (connect_forked unwrap: "opening the extent store at …/elsewhere/store: EBUSY") and cladefsd::tests::restores_over_what_a_failed_cleanup_left_fork_anew (expected EIO, got EBUSY). Instrumented errno_of logged "opening …/store: taking the store's directory: this process has the store open". Both tests drop a CellClient and at once fork a version of its cell. opened_version (crates/cladefs/src/cladefsd/cells.rs) syncs the cell's stores in table().instances(); sync opens one with no connection (`Here::open(&path, hold(&path).await?)`) while the dropped connection's close still holds it, so the open fails as busy. Five interleaved rounds of cladefsd::tests on main and the branch were clean on both; whether TASK-633.9 made it likelier is not shown.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A fork of a version made right after its writer's connection drops waits for that close, or syncs the store it closes, and never fails EBUSY for it
- [x] #2 A test reproduces the window deterministically (a held close), failing before the fix
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Find the window: a store whose last reference is dropped closes on that thread, its directory's claim let go only at the end of the close; the daemon reopening it meanwhile met "this process has the store open".
2. LocalCell::close marks its claim closing first; DiskDir::claim waits for a closing store's claim, bounded, and refuses an open store at once as before.
3. sync uses a live instance under its hold; opened_version keeps no references.
4. A deterministic test with failpoints; a mutant; Linux suite.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Paused 2026-11-18 for TASK-633.2, at the user's word; nothing changed yet.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
An opening of a store this process is closing now waits for its close (up to 60 s) instead of failing EBUSY: LocalCell::close marks its directory claim closing (Dir::closing), and DiskDir::claim waits for such a claim to go, refusing an open store at once as before (stores_open_once). The daemon's sync also uses a live instance under its hold, and opened_version keeps no reference to the stores it syncs. Verified by openings_wait_for_a_store_closing, which holds the close and the opening at failpoints (a mutant that never marks closing fails it), and the Linux workspace suite on ubu1 at 2a7477fed: 2979/2979, the two EBUSY failures of earlier runs gone.
<!-- SECTION:FINAL_SUMMARY:END -->
