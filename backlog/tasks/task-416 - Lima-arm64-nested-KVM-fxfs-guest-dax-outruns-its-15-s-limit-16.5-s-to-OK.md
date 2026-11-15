---
id: TASK-416
title: 'Lima (arm64, nested KVM): fxfs-guest-dax outruns its 15 s limit: 16.5 s to OK'
status: To Do
assignee: []
created_date: '2026-11-14 20:55'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 807000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
In Lima on an M5 Pro (nested KVM, VM kernel with kvm-arm.vgic_v3_group1_trap=1), fxfs-guest-dax's guest passes every check but says OK at 16.5 s, past the case's 15 s (outcome timeout, took 19.5 s). Boot alone is ~4 s there (2.5-4.0 s to the agent), against ubu1's x86-64 budgets. Unknown how much is nested virtualization's cost, the group 1 EOI trap's (every guest EOI exits to the VM's KVM), or arm64. Every other guest case is unrun on Lima. No time-scaling knob exists for the runner's limits; adding one is a design choice (the user's rule: no timeouts raised blindly), so measure first.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Where fxfs-guest-dax's time goes in Lima is measured: boot, each check, against ubu1's
- [ ] #2 The guest cases' e2e tag passes in Lima on arm64, or each that can't is listed with why
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
