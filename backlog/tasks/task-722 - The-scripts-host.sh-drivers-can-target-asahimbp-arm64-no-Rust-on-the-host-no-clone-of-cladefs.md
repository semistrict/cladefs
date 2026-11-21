---
id: TASK-722
title: >-
  The scripts/*-host.sh drivers can target asahimbp: arm64, no Rust on the host,
  no clone of cladefs
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-21 07:02'
updated_date: '2026-11-21 07:47'
labels:
  - asahimbp
dependencies: []
priority: medium
ordinal: 1126000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Since 2026-11-21 agents send each run to the less loaded of ubu1 and asahimbp (AGENTS.md, "Linux runs go to the least loaded test host"). The drivers (scripts/lib/host.sh: test-host.sh, bench-host.sh, mutants-host.sh, ...) assume an x86-64 host with the user's Rust in ~/.cargo and a git clone at src/fxdax fetching from origin. asahimbp (Fedora Asahi, arm64, 16K pages, 11 CPUs, 17 GB) has fxjob and /mnt/fxdax-scratch (TASK-720) but runs builds only in the localhost/fxdax-rust podman image, and its ~/src/fxdax is an rsync copy. So every driver run goes to ubu1 however loaded it is: on 2026-11-21 ubu1 had three jobs queued (one waiting 17 min) while asahimbp sat idle.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 CLADEFS_TEST_HOST=asahimbp scripts/test-host.sh runs the workspace suite there and copies its results back
- [x] #2 A driver whose run cannot work on an arm64 host (x86-64 guest kernel, its guest cases) says so before starting rather than failing mid-run
- [x] #3 asahimbp's setup (~/setup/scripts there) installs what the drivers need, not a run
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 scripts/upstream-changes.py --check passes
- [ ] #2 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. asahimbp setup step 82-cladefs-dev.sh (the user's ~/setup there): Fedora packages, rustup stable + nextest pinned, ~/src/fxdax made a clone in place. 2. host.sh host_need_arch ARCH WHY; host-remote.sh prepare --arch checks uname -m before making anything. 3. Guest-booting drivers declare x86_64 (test-host.sh unless CLADEFS_GUEST_TESTS=0, bench, bench-ab, migrate, python-sandbox); host_setup takes arm64, host_kernel refuses non-x86-64. 4. Tests: fake uname in fake_ssh and test_linux_host; refusal and arm64 workspace-only cases. 5. Real runs on asahimbp: refusal, then the workspace suite.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Pushed adb4ef14a. Real refusal on asahimbp (CLADEFS_TEST_HOST=asahimbp scripts/test-host.sh): exit 2, "test-host.sh: the guest tests boot an x86-64 guest kernel (CLADEFS_GUEST_TESTS=0 runs the workspace's tests alone): this test host is aarch64, the run needs x86_64"; no run directory or worktree made. Workspace suite run t722 started (queued behind another session's 12G job there).
<!-- SECTION:NOTES:END -->
