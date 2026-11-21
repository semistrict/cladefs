---
id: TASK-719
title: >-
  Cargo registry source of proptest 1.11.0 was edited on the Mac (prop_assert_eq
  awaiting its left side)
status: To Do
assignee: []
created_date: '2026-11-21 06:02'
labels:
  - testing
dependencies: []
priority: medium
type: bug
ordinal: 1120000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
On 2026-11-21 at 00:58 something edited ~/.cargo/registry/src/index.crates.io-*/proptest-1.11.0/src/sugar.rs, line 794: prop_assert_eq!'s two-argument arm read `let left = $left.await;`. Cargo assumes registry sources immutable, so targets built before kept the pristine build, and any target built after failed open_cell's serves_whatever_a_guest_asks with E0728 (await outside async), which looked like a broken main. Found landing TASK-702.2; restored from the .crate archive (one-line diff) and `cargo clean -p proptest` in the landing tree. Likely an agent's async rewrite (TASK-685) sed-editing past the repo. Other targets built since 00:58 hold the tampered proptest until cleaned.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Whatever edited the registry is found and stopped (a script or agent instruction), so dependency sources are never edited in place
- [ ] #2 Targets on the Mac and ubu1 built from the tampered source are cleaned of proptest
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 scripts/upstream-changes.py --check passes
- [ ] #2 Deviations from upstream documented where they occur
<!-- DOD:END -->
