---
id: TASK-657
title: >-
  fxfs-guest fails on ubu1: Firecracker opening its extent store gets EACCES,
  the VM never starts
status: To Do
assignee: []
created_date: '2026-11-19 14:29'
labels:
  - test-host
  - guest
dependencies: []
priority: high
type: bug
ordinal: 1036000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Since 2026-11-19, the guest suite's first case, fxfs-guest, fails on ubu1 in 0.4-2 s: Firecracker exits building the microVM, 'CreateExtentDevice ... opening the extent store at .../results/e2e/fxfs-guest/fxfs-guest/extents: EACCES (errno 13)'; console and stdout empty; cladefsd.log says nothing of it (the daemon answered EACCES to the VM's open). The 15 other guest cases pass in the same run, fxfs-guest-crash among them. Seen four times: branch t3/mirrors at 50933bf33, 439a7068c, de80816c7, and main's 348c59327 (the branch's base), whose code is that of 018345e26, which passed every Linux part on 2026-11-18 (backlog commits only since). So something on the test host changed, or the case depends on state the host keeps between runs. Replay: CLADEFS_WORKSPACE_TESTS=0 CLADEFS_E2E_TEST=fxfs-guest CLADEFS_TEST_HOST_COMMIT=348c59327 scripts/test-host.sh RESULTS (results of the base run in /tmp/mirrors-guest-base on the Mac that ran it).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The cause of the EACCES is found and named
- [ ] #2 fxfs-guest passes on ubu1 at main's head
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
