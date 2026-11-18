---
id: TASK-647
title: >-
  sandboxes (short) on main's CI: both pgbench VMs stop after their load,
  exiting 0 without OK sandbox pgbench
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18 20:37'
updated_date: '2026-11-18 22:08'
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
- [ ] #1 Why the pgbench role ends after its load without its runs or OK sandbox pgbench is found and recorded here, from the guest's console and the role's code.
- [ ] #2 sandboxes.sh short passes on main's CI and on ubu1, and a pgbench role that stops early still fails its check.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Cause (AC #1), ubu1 2026-11-18, sandboxes.sh short with results kept, sandbox-1-pgbench.console.txt: after the load and its first 60 s run, check_data's query fails: ERROR: relation "pgbench_cells" does not exist; the guest panics (test_fxfs_guest_pgbench.rs check_data), reboots, and Firecracker exits 0, its stdout 'OK load 1000000' alone. f3cf366ce (TASK-508.3, branch is cell everywhere) renamed pgbench's own table pgbench_branches to pgbench_cells in test_fxfs_guest_pgbench.rs and test_fxfs_guest_soak.rs, with pgbench's prose (its teller and its branch); hidden on CI from 845a6c123 to 8111fb784 by the guest kernel not compiling. The same rename turned AGENTS.md's git branches into cells. Restored all of them. The check already fails a pgbench role that stops early (it caught this).

A sweep of the rename commits' 362 files (f3cf366ce, 8e9f18a42, ddbe40889; a finder and a skeptic per group) found 35 more words of other systems renamed branch -> cell, all restored: git's in guest cases that ran git (test_fxfs_guest_shared_volumes: init --initial-cell, git cell -D side, twice; test_fxfs_guest_git_fetch: init --initial-cell), which broke those cases; and prose: Firecracker's and this repo's git branches (docs/code.md, docs/testing.md, docs/research/kvm-async-pf..., README.md, OPEN_QUESTIONS.md, build.rs's HEAD), kernel code paths (async branch), upstream Fxfs's merge_into branches (crates/fxfs/src/lsm_tree/merge.rs, an upstream deviation), fuchsia_merkle's branch check.
<!-- SECTION:NOTES:END -->
