---
id: TASK-424
title: >-
  extent_store sim: collection_needs_candidates_first finds no seed under the
  head (TASK-407)
status: To Do
assignee: []
created_date: '2026-11-15 05:07'
updated_date: '2026-11-15 05:39'
labels:
  - sim
  - task-407
dependencies: []
priority: high
ordinal: 817000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
After TASK-407's head design, every holds profile of extent_store's simulation passes, and in the sim cargo profile (scripts/test.sh's sim part, macOS, 2026-11-15) every needed-rule search finds its seed but sim::collection_needs_candidates_first: 'no seed of 0..20000 needed the rule' (706 s). specs/bucket's NoCandidatesFirst still finds its violation in TLC, so the rule is needed; the simulation's profile likely no longer reaches the case (e.g. a fence or a void segment it relied on). Find a profile or seed range that makes it under the head.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
