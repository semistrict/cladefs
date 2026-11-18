---
id: TASK-647
title: >-
  sandboxes (short) on main's CI: both pgbench VMs stop after their load,
  exiting 0 without OK sandbox pgbench
status: Done
assignee:
  - '@claude'
created_date: '2026-11-18 20:37'
updated_date: '2026-11-18 23:59'
labels:
  - testing
  - ci
dependencies: []
priority: medium
type: bug
ordinal: 1031000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
CI runs 44353952388 (8111fb784) and 44380042393 (ca72b5814) on main, 2026-11-18: sandboxes / sandboxes (short) fails in both the same way. scripts/sandboxes.sh short, 8 VMs on 4 CPUs: every VM's checks of memory, uploads, disk and framing pass, but "check failed: vm: sandbox-1-pgbench exited exit status: 0, saying \"OK load 1000000\\n\"", and the same for sandbox-5-pgbench. test_fxfs_guest_sandboxes.rs's finish() wants a line "OK sandbox pgbench". The pgbench guests print their load and then nothing more, yet exit 0 after about 86 s, while the other roles print their commands and OK lines. Not seen on ubu1 yet: the Fix-mutants session's run there couldn't start (TASK-643).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Why the pgbench role ends after its load without its runs or OK sandbox pgbench is found and recorded here, from the guest's console and the role's code.
- [x] #2 sandboxes.sh short passes on main's CI and on ubu1, and a pgbench role that stops early still fails its check.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Reproduce on ubu1 with the run's results kept; read the pgbench VM's console.
2. Fix the cause; sweep the rename commits for other words of other systems it reached.
3. sandboxes.sh short on ubu1 and on main's CI.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Cause (AC #1), ubu1 2026-11-18, sandboxes.sh short with results kept, sandbox-1-pgbench.console.txt: after the load and its first 60 s run, check_data's query fails: ERROR: relation "pgbench_cells" does not exist; the guest panics (test_fxfs_guest_pgbench.rs check_data), reboots, and Firecracker exits 0, its stdout 'OK load 1000000' alone. f3cf366ce (TASK-508.3, branch is cell everywhere) renamed pgbench's own table pgbench_branches to pgbench_cells in test_fxfs_guest_pgbench.rs and test_fxfs_guest_soak.rs, with pgbench's prose (its teller and its branch); hidden on CI from 845a6c123 to 8111fb784 by the guest kernel not compiling. The same rename turned AGENTS.md's git branches into cells. Restored all of them. The check already fails a pgbench role that stops early (it caught this).

A sweep of the rename commits' 362 files (f3cf366ce, 8e9f18a42, ddbe40889; a finder and a skeptic per group) found 35 more words of other systems renamed branch -> cell, all restored: git's in guest cases that ran git (test_fxfs_guest_shared_volumes: init --initial-cell, git cell -D side, twice; test_fxfs_guest_git_fetch: init --initial-cell), which broke those cases; and prose: Firecracker's and this repo's git branches (docs/code.md, docs/testing.md, docs/research/kvm-async-pf..., README.md, OPEN_QUESTIONS.md, build.rs's HEAD), kernel code paths (async branch), upstream Fxfs's merge_into branches (crates/fxfs/src/lsm_tree/merge.rs, an upstream deviation), fuchsia_merkle's branch check.

ubu1 2026-11-18 22:24, tree 65568da06 (the fix and main), prebuilt, fxjob --bench --mem 16G: scripts/sandboxes.sh short exit 0, stores 1.22x what the guests wrote. A run just before failed in a host-wide stall beside others' swapping jobs (fsyncs of 500 s), the writes 1.26x: not this bug. The check that caught this (finish(), each role's OK sandbox line) is what fails a pgbench role that stops early.

Main's CI at a06c690a6 failed both jobs for other reasons: sandboxes built no Firecracker, vendor/firecracker.lock behind 97f0dead8's storage_device libc (regenerated, 2ff7ca286); scripts' test_nightly test_a_night_that_measured_nothing_is_measured_again: nights of one head within a second named their results alike, mkdir File exists (nightly-host.sh now counts the name on, -2, -3; a test takes every name of the next two minutes and fails without it as CI did).

Main's CI run 44419894015 (9f8ae8dbb, the fixes in): sandboxes (short) success, test (clippy scripts) success. Workspace: macOS 2641/2641 and Linux 2962/2962 on this branch's tree before the string fixes, which touch only guest cases' SQL and git arguments, prose and comments, and scripts/nightly-host.sh (its tests 16 OK). upstream-changes.py --check passes (the merge.rs comments restored are upstream's words again).
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
The sandboxes' pgbench VMs stopped after their load because f3cf366ce's branch -> cell rename renamed pgbench's own table pgbench_branches to pgbench_cells: check_data's query failed, the guest panicked and rebooted, and Firecracker exited 0. Restored, and 35 more of other systems' words the rename reached (git commands in the shared-volumes and git-fetch guest cases, which git refused; Firecracker's and this repo's git branches in docs; upstream Fxfs's merge_into comments; AGENTS.md's rule on branches). On the way to a green CI: vendor/firecracker.lock regenerated for 97f0dead8's storage_device libc, and nightly-host.sh's same-second result names counted on (with a test that fails without it). sandboxes.sh short passes on ubu1 (1.22x) and main's CI (run 44419894015), as does the scripts job.
<!-- SECTION:FINAL_SUMMARY:END -->
