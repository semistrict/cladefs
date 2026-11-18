---
id: TASK-632
title: >-
  Six of cladefs's hand mutants no longer apply on main: mutants.sh --check
  fails
status: Done
assignee:
  - '@claude'
created_date: '2026-11-18 16:45'
updated_date: '2026-11-18 22:22'
labels:
  - testing
dependencies: []
priority: high
type: bug
ordinal: 948000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/cladefs/mutants.sh --check, which scripts/test.sh's scripts part runs (CI's clippy scripts job), fails at a1167a56a with 6 of 22 broken: a-second-open-holds-its-path and tenants-name-branches (cladefsd/server.rs), copies-of-whole-requests (src/extents.rs, gone), forks-take-branches-claimed (host_store.rs and bucket_cell/src/branch.rs, gone since TASK-508's renames), framed-in-copies and large-extents-framed-in-copies (local_cell/src/store.rs, gone: the store is cell.rs). Found while retargeting the disk mutants for TASK-539. Each is to be retargeted at the code that now does what it broke, and still killed by its test, or, if its subject is gone, said so and replaced.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 crates/cladefs/mutants.sh --check reports no patch broken
- [x] #2 Each retargeted mutant is killed by the test its header names (mutants.sh NAME on ubu1)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Retarget each broken patch at the code that now does what it broke, -F0, same killer; replace those whose subject is gone (tenants-name-branches, forks-take-branches-claimed).
2. Adversarial review of each retargeting.
3. mutants.sh --check: none broken; each killed by its killer on ubu1; large-extents-framed-in-copies' killer given back the large extents it lost; framed-in-copies' script killer run build-then-measure (TASK-649), its baseline fixed first (TASK-647).
4. Merge origin/main, push.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-18, origin/main 03b655f63: a seventh, tenants-name-any-store (cladefsd/server.rs, since ab5226324's grant serialization), also no longer applies. TASK-539's branch retargets walks-forget-writes-in-flight (as a-stores-reserve-forgotten) and the other disk mutants.

2026-11-18, branch t3/retarget-broken-mutants (149954227): drift retargeted: a-second-open-holds-its-path, tenants-name-any-store (prose: TASK-364's reaches now refuses another tenant's closed store; the kill is its creates in alice's directory), copies-of-whole-requests (PIECE is OpenCell's, open_cell.rs, since 8e9f18a42), framed-in-copies and large-extents-framed-in-copies (seal_own's in-place skip, local_cell/src/cell.rs). Subjects gone, replaced: tenants-name-branches -> restores-make-others-cells (Policy::names_branches and ForkFrom's branch gone in 9a65b3838; a restore's child now takes the restoring tenant, self.tenant.granted(), which the mutant drops so the version's tenant, named by the VM process, gets the cell); forks-take-branches-claimed -> forks-take-cells-claimed (record_fork, child_free and the look gone in d0bc6fd47; lifecycle::fork_version_keyed's IdTaken arm is now the one refusal). scripts/rust-rules/unruled kept in step. large-extents-framed-in-copies' killer, bucket::write_budgets::a_hosts_store_writes_what_its_design_says, had written no extent of large_extent or more since 89f518bc0, so seal_own never ran and the mutant went unkilled: the test gains a large-extents workload (4 x 8 MiB, own files' writes bounded by their tails: 1,716 bytes of 149,304 allowed). Kills on ubu1 (fxjob, crates/cladefs/mutants.sh): a-second-open-holds-its-path 40s ('a connection opened 2 stores'), tenants-name-any-store 39s (alice's listing changed), restores-make-others-cells 28s (reopen of the restored store refused), copies-of-whole-requests 42s (MiB held past the credit's 272 MiB), forks-take-cells-claimed 36s, large-extents-framed-in-copies 66s ('large extents' own files written 33556148 bytes: 149304 at most'). Unmutated write_budgets passes on Linux (io check) and macOS.

Pushed to main as b454b96f6 (merged origin/main 6b7b54e24; every mutants.sh --check 0 broken after it). Linux workspace on ubu1 (fxjob, scripts/test.sh workspace, --no-fail-fast): 2962/2962 passed; a first run's one failure, cladefsd::tests::hostile::page_ins_in_flight_are_bounded (EBUSY reopening the restored store after its fork's connection dropped), passed 20 of 20 alone and is TASK-648's race. macOS: test.sh clippy + workspace 2641/2641. framed-in-copies' killer, scripts/sandboxes.sh short, can't build inside fxjob --bench since TASK-368 (measurement_refuses): its kill runs as build-then-measure, artifacts built as work (CLADEFS_E2E_BUILD_ONLY, the mutant's through patch_run), then sandboxes.sh under --bench with CLADEFS_E2E_PREBUILT.

framed-in-copies' kill, 2026-11-18 21:3x: artifacts built as work (unmutated, and the mutant's through patch_run); the first prebuilt sandboxes run skipped its case (CLADEFS_GUEST_KERNEL unset, no host kernel cache entry for this tree's key release-f9dd6e4aebc7e837), so its 'killed' meant nothing; the guest kernel is building, then the unmutated baseline and the kill run again.

framed-in-copies: prebuilt, on the ext4 scratch disk with 16G, under fxjob --bench (2026-11-18 21:47): the unmutated tree fails sandboxes.sh short too, both pgbench VMs exiting 0 after 'OK load 1000000' (main's TASK-647), so the kill can't be told from the baseline until TASK-647 is fixed. Kept open for it. (An 8G run's mutant 'killed' on the run's memory check, and one without a guest kernel skipped the case: neither counts.)

framed-in-copies killed (ubu1, 2026-11-18 22:19, tree 65568da06, artifacts built as work, the mutant's through patch_run, then fxjob --bench --mem 16G with CLADEFS_E2E_PREBUILT on the ext4 scratch disk, guest kernel of the tree's key 11cfcc70d34c9db6): 'writes: the stores wrote 1.92 times what the guests did, past 1.5', segments_framed_in_place=0 on both hosts (TASK-411.23 recorded 1.83-1.91). The unmutated tree on the same host and artifacts, 22:09: 1.26 times, 37 segments framed in place (21 + 16); its run failed otherwise, a host-wide stall (fsyncs of 500 s, the daemons runnable 135 s unscheduled) beside others' swapping jobs, not the writes check.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
All 22 of cladefs's hand mutants apply on main again (mutants.sh --check 0 broken) and each retargeted one is killed by its killer on ubu1. Drift retargeted: a-second-open-holds-its-path, tenants-name-any-store, copies-of-whole-requests, framed-in-copies, large-extents-framed-in-copies. Subjects gone, replaced with the same bug in the code that took their place: restores-make-others-cells (was tenants-name-branches) and forks-take-cells-claimed (was forks-take-branches-claimed). bucket::write_budgets::a_hosts_store_writes_what_its_design_says writes large extents again, as it hadn't since 89f518bc0, so large-extents-framed-in-copies is killed (33,556,148 bytes against 149,304). framed-in-copies killed by sandboxes.sh short's writes check (1.92x against the tree's 1.26x), after TASK-647's fix to the baseline. Workspace: macOS 2641/2641, Linux 2962/2962. Pushed to main.
<!-- SECTION:FINAL_SUMMARY:END -->
