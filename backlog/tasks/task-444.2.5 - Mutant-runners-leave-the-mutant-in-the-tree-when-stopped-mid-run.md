---
id: TASK-444.2.5
title: Mutant runners leave the mutant in the tree when stopped mid-run
status: Done
assignee: []
created_date: '2026-11-15 19:03'
updated_date: '2026-11-15 19:17'
labels:
  - bug
  - testing
dependencies: []
parent_task_id: TASK-444.2
priority: high
ordinal: 871000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by Codex's review of 3b64d68ec (scripts/review-change.sh). scripts/loom-mutants.sh and crates/{cladefs,extent_remote,fxfs-posix}/mutants.sh apply a mutant with patch, run its killer, then reverse it, with no trap: a SIGTERM or ^C during the build or test (a cancelled run) exits before the reversal, and the tree keeps the weakened code (e.g. Latest::rise with Relaxed), built and possibly committed from then on. Nothing checks it: test_loom_mutants only checks restoration after a normal return. crates/fxfs-sim-diff/mutants.sh patches a copy and is safe.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A runner stopped by SIGTERM or SIGINT while its killer runs leaves the tree as it was, and stops the killer
- [x] #2 A mutant left by a runner stopped with SIGKILL is taken out by the next runner before it starts, or the runner refuses while a live runner holds the tree
- [x] #3 One helper in scripts/lib/patches.sh does it for all four runners; a test shows each case red without it
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Fixed in 7f68b3dc9.

How found: Codex's second-vendor review of 3b64d68ec (scripts/review-change.sh); its reproducer (stop a runner mid-killer, check the tree) is now test_patches.Runners.

Blind spot: the runners' tests only checked the tree after a normal return; nothing ever stopped a runner mid-run.

Class test: test_patches.PatchRun (SIGTERM, SIGINT, SIGKILL then recovery, a live holder refused) and Runners: each of the four runners stopped by SIGTERM while a blocking fake killer runs leaves its tree as it was and its killer stopped (red on all four before: exit -15, killer left running, mutant in the tree); and a lint that no crates/*/*.sh, scripts/*.sh or guest/**/*.sh applies a patch with -p1 to its own tree (no -d) except through patch_run (red before: 8 lines).

Look around: crates/fxfs-sim-diff/mutants.sh and guest/sim/lib.sh patch copies (-d), safe. scripts/mutants.sh uses cargo-mutants, which copies the tree. build-guest-kernel.sh applies to a git index, not the tree.

Structure: four runners each repeated apply/run/reverse; one owner (patch_run) now holds it, so a new runner can't forget the reversal, and the lint keeps it so.

End to end: real scripts/loom-mutants.sh latest-rises-relaxed stopped by SIGTERM mid-build: hearing.rs back as it was, no cargo left; then latest-rises-relaxed and latest-listens-relaxed run to completion, both killed. scripts/tests: 360 tests OK on macOS.
<!-- SECTION:NOTES:END -->
