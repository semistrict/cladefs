---
id: TASK-149.47
title: >-
  guest/sim: patch-0010's mutant no longer killed: interleave misses it in 2,000
  seeds; needs a scenario
status: Done
assignee: []
created_date: '2026-11-15 20:54'
updated_date: '2026-11-15 20:57'
labels:
  - bug
  - testing
  - guest-sim
dependencies: []
parent_task_id: TASK-149
priority: high
ordinal: 872000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
guest/sim/mutants.sh on main (02af2d9d5, 2026-11-15, the Linux container): patch-0010-reads-replace-their-entries MISSED, interleave passing 2,000 seeds (288 s); TASK-149.14 had it killed at seed 610. Same class as TASK-149.26: a catch resting on one seed's interleaving, lost once later changes moved seeds. It fails the DAX gate's first step (scripts/dax-gate.sh runs every sim mutant), so every guest DAX change is blocked on it. The mutant's header: a read fault of a page another mapping holds, its blocks shared, replaces the entry naming that page, unmapping it from every mapping: two processes reading one page undo each other's mappings; kernel/mm.c's check (a load faults again on a page its mapping held only after something other than a fault unmapped it) finds it.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A scenario plays out two mappings of one shared-block page read in turn and fails with patch 0010 out, in seconds
- [x] #2 patch-0010-reads-replace-their-entries's Killer is that scenario, and guest/sim/mutants.sh kills it
- [x] #3 The other seeded killers (interleave, failures) checked for the same: each mutant they kill is killed within its seeds, or gets a scenario or a task
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Fixed in 4898943d5 (branch land-444-6).

How found: running the DAX gate's first step (guest/sim/mutants.sh, the Linux container) for TASK-444.6; then on a worktree of main 02af2d9d5: MISSED there too (288 s, 2,000 seeds).

Blind spot: a mutant whose only killer is a seeded run rests on one seed's interleaving, which any change moving seeds loses (TASK-149.26's class); nothing checked the full mutant run but the gate.

Class test: shared_reads_stay, a scenario playing the interleaving out: a clone makes the file's page a region slice (shared read), two mappings of it loaded in turn must fault once each; asserts window_regions == 1 so that it stays a test of the shared path. Killed with patch 0010 out in 0 s; passes clean; sweep: 36 calls, 0 failed.

Look around: the same run's other seeded killers: task-160.2-resize-no-rollback (interleave seed 1), task-149.18-round-unwoken (seed 5), task-147-growing (seed 24), task-79.53-gap-left (failures seed 4): all within seeds. Left on seeds still: those four; each is a candidate for a scenario before the next seed shift (TASK-149.26's advice).

Structure: guest/sim's CI jobs run mutants.sh --check (they apply) but not the mutants themselves; only dax-gate.sh does, so a lost kill shows up only when someone runs the gate. Not changed here: CI's time budget is the user's decision (096fa2233).
<!-- SECTION:NOTES:END -->
