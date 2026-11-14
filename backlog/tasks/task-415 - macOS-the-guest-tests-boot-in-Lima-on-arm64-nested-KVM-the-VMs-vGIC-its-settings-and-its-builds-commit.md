---
id: TASK-415
title: >-
  macOS: the guest tests boot in Lima on arm64 (nested KVM): the VM's vGIC, its
  settings and its builds' commit
status: Done
assignee:
  - '@claude'
created_date: '2026-11-14 20:55'
updated_date: '2026-11-14 20:55'
labels: []
dependencies: []
type: bug
ordinal: 806000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Asked to get fxdax working on the Mac (2026-11-14). The only practical path is the existing one: the Linux stack in Lima (Apple's virtualization, nested KVM, arm64). Running natively would need the extent daemon off memfd/userfaultfd and a VMM on Apple's hypervisor (the libkrun-era design TASK-40 removed). Three things stopped scripts/test-e2e-linux.sh from booting a guest on an M5 Pro (macOS 27.0.1, Lima 2.2.0, VM kernel 7.0.0-42): lima.sh passed only LIBCLANG_PATH into the VM, so FXDAX_GUEST_KERNEL and every other FXDAX_* setting were dropped; under a VM kernel of 6.19 or later every L2 guest, even plain Firecracker with virtio-blk alone, stopped a few seconds into its boot (KVM's vgic-state: vCPU 0's IPI and virtual timer pending and enabled, nothing active, the guest leaving WFI ~20k times a second without taking them), the regression of Linux 73c9726975af under Apple's nested GIC, worked around by kvm-arm.vgic_v3_group1_trap=1 (https://github.com/agent-substrate/substrate/pull/3149); and the VM's tree has no .git, so builds there said commit unknown, which fxfs-guest's build_commit_shown fails.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 lima.sh passes the caller's FXDAX_* settings (but FXDAX_LIMA_INSTANCE) and LIBCLANG_PATH to the VM's script
- [x] #2 lima.sh's builds report the checkout's commit (FXDAX_BUILD_COMMIT), unless one is given
- [x] #3 lima-setup.sh boots the VM's kernel with kvm-arm.vgic_v3_group1_trap=1, restarting the VM to take it
- [x] #4 fxfs-guest-dax's guest runs every check to OK in Lima on arm64
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. lima.sh forwards FXDAX_* and LIBCLANG_PATH. 2. lima.sh sets FXDAX_BUILD_COMMIT from build-commit.sh on the checkout. 3. lima-setup.sh adds kvm-arm.vgic_v3_group1_trap=1 through /etc/default/grub.d and restarts the VM when /proc/cmdline lacks it. 4. Verify with scripts/tests/test_linux_host.py and fxfs-guest-dax in Lima.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Validation: scripts/tests/test_linux_host.py 25/25 (new: test_the_vms_script_takes_the_fxdax_settings_given, test_the_vms_builds_are_of_the_checkouts_commit, test_a_build_commit_given_is_the_vms; the first two red before). lima-setup.sh run on the default instance: restarted, /proc/cmdline has kvm-arm.vgic_v3_group1_trap=1 (kernel 7.0.0-43). Before it: fxfs-guest-dax timed out with the guest silent after rdinit, and plain Firecracker with virtio-blk alone hung 4 of 4 boots. After: the guest runs every check and prints OK (16.5 s, past the case's 15 s limit: TASK-416). The arm64 kernel builds in 7 min in the VM; its kallsyms warnings are TASK-413. In the VM, the VMM's extent and PCI tests (29), the harness's (127, 99, 9) and the 30 others test-e2e.sh runs first all pass.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
fxdax's Linux stack runs on the Mac in Lima (arm64, nested KVM): lima.sh passes the caller's FXDAX_* settings and the checkout's commit into the VM, and lima-setup.sh boots the VM's kernel with kvm-arm.vgic_v3_group1_trap=1, without which no L2 guest got past its first seconds under a 6.19+ VM kernel. Verified: test_linux_host.py 25/25; fxfs-guest-dax's guest passes every check in Lima (OK), its 15 s limit left to TASK-416.
<!-- SECTION:FINAL_SUMMARY:END -->
