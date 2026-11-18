---
id: TASK-647
title: >-
  sandboxes (short) on main's CI: both pgbench VMs stop after their load,
  exiting 0 without OK sandbox pgbench
status: To Do
assignee: []
created_date: '2026-11-18 20:37'
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
