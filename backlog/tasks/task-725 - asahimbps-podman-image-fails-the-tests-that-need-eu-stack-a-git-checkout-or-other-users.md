---
id: TASK-725
title: >-
  asahimbp's podman image fails the tests that need eu-stack, a git checkout or
  other users
status: To Do
assignee: []
created_date: '2026-11-21 07:39'
labels:
  - test-env
  - arm64
dependencies: []
priority: low
ordinal: 1130000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen 2026-11-21, on the pre-async base too. In localhost/fxdax-rust on asahimbp: stacks::tests::stacks_name_where_threads_wait ('sh: exec: eu-stack: not found'), the cladefsctl sigquit stack tests (time out at 60 s, the same), tests::builds_are_stamped_with_their_commit (the tree is an rsync copy, no .git), cladefs::grants' three socket tests (other users, in a rootless container). Fix in the host's image/setup (elfutils in the Containerfile; a run's way to see other uids), not in a run. Related: TASK-722.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 scripts/upstream-changes.py --check passes
- [ ] #2 Deviations from upstream documented where they occur
<!-- DOD:END -->
