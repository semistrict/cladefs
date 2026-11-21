---
id: TASK-722
title: >-
  The scripts/*-host.sh drivers can target asahimbp: arm64, no Rust on the host,
  no clone of cladefs
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-21 07:02'
updated_date: '2026-11-21 07:38'
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
- [ ] #2 A driver whose run cannot work on an arm64 host (x86-64 guest kernel, its guest cases) says so before starting rather than failing mid-run
- [ ] #3 asahimbp's setup (~/setup/scripts there) installs what the drivers need, not a run
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 scripts/upstream-changes.py --check passes
- [ ] #2 Deviations from upstream documented where they occur
<!-- DOD:END -->
