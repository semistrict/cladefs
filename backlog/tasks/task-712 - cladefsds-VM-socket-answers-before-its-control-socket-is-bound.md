---
id: TASK-712
title: cladefsd's VM socket answers before its control socket is bound
status: Done
assignee:
  - '@claude'
created_date: '2026-11-20 22:47'
updated_date: '2026-11-21 02:17'
labels:
  - bug
  - flake
dependencies: []
priority: medium
ordinal: 1115000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
boot bound the VM socket, then made the tenants directory and bound the control socket: what waits for the daemon by connecting to its VM socket, as cladefs/tests/peers.rs's daemon() does, then connected to the control socket before it was there (peers_are_set_at_runtime: No such file or directory). Seen once the tenants directory was made durably (TASK-708), its syncs widening the window. Fix: the VM socket bound last, once all else serves.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
cladefsd binds its VM socket last, once the control API serves. Verified: peers_are_set_at_runtime passes on ubu1, unit 3062. Commit 385446f82.
<!-- SECTION:FINAL_SUMMARY:END -->
