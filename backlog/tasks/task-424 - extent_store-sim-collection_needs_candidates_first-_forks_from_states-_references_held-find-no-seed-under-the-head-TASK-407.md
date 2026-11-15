---
id: TASK-424
title: >-
  extent_store sim: collection_needs_candidates_first, _forks_from_states,
  _references_held find no seed under the head (TASK-407)
status: To Do
assignee: []
created_date: '2026-11-15 05:07'
labels:
  - sim
  - task-407
dependencies: []
priority: high
ordinal: 817000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
After TASK-407's head design (and the fork-point checkpoint fix, the pass keeping checkpoint(parent, at, epoch_of(at))), every holds profile of extent_store's simulation passes on macOS (dev profile), but three needed-rule searches run past 25 minutes without a seed violating with their rule off: sim::collection_needs_candidates_first, sim::collection_needs_forks_from_states, sim::collection_needs_references_held. specs/bucket's NoCandidatesFirst, NoForksFromStates and NoReferencesHeld still find their violations in TLC, so the rules are needed; the profiles likely no longer reach the case (e.g. a fence or a void segment they relied on). Find a profile (or seed range) that makes each case under the head, in the sim cargo profile.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
