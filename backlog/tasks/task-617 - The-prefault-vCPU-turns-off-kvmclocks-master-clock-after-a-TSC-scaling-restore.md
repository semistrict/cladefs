---
id: TASK-617
title: >-
  The prefault vCPU turns off kvmclock's master clock after a TSC-scaling
  restore
status: To Do
assignee: []
created_date: '2026-11-18 15:34'
labels: []
dependencies: []
priority: low
type: bug
ordinal: 1008000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), firecracker review; verified against the code by independent reviewers. Severity: low.

**Where:** `vendor/firecracker/src/vmm/src/devices/virtio/extent/window.rs`: `rg -n -F 'let vcpu = vm.fd().create_vcpu(id).map_err(io::Error::other)?;' vendor/firecracker/src/vmm/src/devices/virtio/extent/window.rs`

build_microvm_from_snapshot scales each guest vCPU with the per-vCPU KVM_SET_TSC_KHZ when the snapshot's tsc_khz differs from the host's by more than 250 ppm. It then restores their MSR_IA32_TSC, so kvm->arch.last_tsc_khz becomes the scaled frequency. Device restore runs after that: restored_extent_prefault calls KvmPrefault::new, which creates one more vCPU. It gets the VM's default_tsc_khz (the host's), and only its CPUID is set. KVM's postcreate kvm_synchronize_tsc(vcpu, NULL) counts as synchronizing, but `virtual_tsc_khz == last_tsc_khz` is false. So the vCPU starts a new TSC generation, nr_vcpus_matched_tsc drops to 0, and kvm_track_tsc_matching sees nr_vcpus_matched_tsc + 1 != online_vcpus and disables use_master_clock. The guest vCPUs never write the TSC again, so the master clock stays off for the VM's life. kvmclock then lacks PVCLOCK_TSC_STABLE_BIT. A kvm-clock guest (CONFIG_KVM_GUEST=y in the guest config) loses vDSO clock_gettime (each call becomes a syscall) and pvclock reads go through the global last_value cmpxchg. With matching frequencies the creation matches, so only a scaled restore is hit. A scaled restore is exactly a fork of a version on another host of a different TSC frequency.

**Trigger.** Take a VM snapshot with a DAX window on host A. Restore it on host B whose TSC kHz differs by more than 250 ppm: is_tsc_scaling_required is true, the vCPUs are set to A's kHz, MSRs are restored, and DeviceManager::restore creates the prefault vCPU at B's kHz. The new TSC generation disables the master clock, and every clock read in the guest is slower for as long as it runs.

**Why the tests stay green.** Every guest case restores on the same test host (ubu1), so TSC scaling is never required and the prefault vCPU's frequency always matches. No test checks kvmclock's stable flag or vDSO use after a restore.

**Backlog.** Related to TASK-27.2 (Done), which built the prefault vCPU, and TASK-271 (To Do), a test_set_tsc failure on GCE. Neither covers TSC scaling on restore.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 After a VM snapshot is restored with a saved tsc_khz about 1% off the host's (so KVM scales it), a VM with a DAX window keeps kvmclock's master clock: its pvclock pages carry PVCLOCK_TSC_STABLE_BIT, as after the same restore without a window.
- [ ] #2 A guest case restores a VM snapshot whose tsc_khz is edited to differ by about 1%, with and without a DAX window, and checks the pvclock stable flag in the guest. It goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
