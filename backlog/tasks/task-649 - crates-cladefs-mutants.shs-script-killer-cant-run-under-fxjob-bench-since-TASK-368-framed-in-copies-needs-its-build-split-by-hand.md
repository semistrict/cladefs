---
id: TASK-649
title: >-
  crates/cladefs/mutants.sh's script killer can't run under fxjob --bench since
  TASK-368: framed-in-copies needs its build split by hand
status: To Do
assignee: []
created_date: '2026-11-18 20:55'
labels:
  - testing
dependencies: []
priority: medium
type: bug
ordinal: 1032000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
framed-in-copies.patch's killer is scripts/sandboxes.sh short, which test-e2e.sh builds inside: under fxjob --bench, as a measurement must run, measurement_refuses it (exit 2, BROKEN); outside it the framings check is timing under others' load. Found by TASK-632, 2026-11-18: run by hand as build-then-measure (CLADEFS_E2E_BUILD_ONLY with the patch in through patch_run, then CLADEFS_E2E_PREBUILT under --bench). mutants.sh should do that itself for a script killer, as scripts/lib/host.sh's host_job splits build and measure.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 crates/cladefs/mutants.sh framed-in-copies under fxjob --bench builds the mutant's artifacts as work, then runs its killer against them, and reports killed
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
